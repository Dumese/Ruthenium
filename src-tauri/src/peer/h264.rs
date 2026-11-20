//! H.264 编码（Media Foundation MFT；硬件优先，软件回退）。
//!
//! 设计：
//! - **编码器**：`MFTEnumEx` 先找硬件 H.264 编码器（同步 MFT），找不到再退回软件 MFT；
//!   两者都不可用（N 版未装媒体功能包）时由调用方继续用 JPEG。
//! - **输入**：BGRA → NV12（BT.601 limited range，CPU 转换；可按 1/0.75/0.5 缩放）。
//! - **输出**：Annex B 字节流（若 MFT 给 AVCC 则自动转 Annex B），每个访问单元一帧，
//!   解析 NAL 类型标记关键帧（IDR=5），时间戳沿用 MFT 的 100ns 时钟。
//! - **低延迟**：`CODECAPI_AVLowLatencyMode`、无 B 帧、GOP=2s；主动请求关键帧用于
//!   WebCodecs 重新同步。
//!
//! 安全/稳定性：编码失败只影响画面（调用方回退 JPEG），不 panic；所有 COM 对象随
//! `H264Encoder` 释放。

use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Annex B 访问单元（可能含 SPS/PPS + IDR + 非 IDR 片）。
    pub data: Vec<u8>,
    pub key: bool,
    /// 微秒（来自 MFT 采样时间）。
    pub ts_us: u64,
}

/// 编码器不可用时的错误（调用方回退 JPEG）。
pub type EncoderResult<T> = Result<T, String>;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;

    use windows::core::Interface;
    use windows::Win32::Foundation::VARIANT_BOOL;
    use windows::Win32::Media::MediaFoundation::{
        eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base, CODECAPI_AVEncCommonMeanBitRate,
        CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncCommonRealTime,
        CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
        CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoMaxNumRefFrame,
        CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate, IMFMediaBuffer, IMFMediaType, IMFSample,
        IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
        MFStartup, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
        MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE,
        MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
        MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
        MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING,
        MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE,
        MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
        MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_VERSION,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
    use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

    /// MFStartup 只做一次（进程级）。
    fn ensure_mf() -> EncoderResult<()> {
        static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
        STARTED
            .get_or_init(|| unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                MFStartup(MF_VERSION, 0).map_err(|e| format!("MFStartup 失败: {e}"))
            })
            .clone()
    }

    fn variant_u32(v: u32) -> VARIANT {
        let mut var = VARIANT::default();
        unsafe {
            let inner = &mut var.Anonymous.Anonymous;
            inner.vt = VT_UI4;
            inner.Anonymous.ulVal = v;
        }
        var
    }

    fn variant_bool(v: bool) -> VARIANT {
        let mut var = VARIANT::default();
        unsafe {
            let inner = &mut var.Anonymous.Anonymous;
            inner.vt = VT_BOOL;
            inner.Anonymous.boolVal = VARIANT_BOOL(if v { -1 } else { 0 });
        }
        var
    }

    /// 把两个 u32 打包到 u64（替代 mfapi.h 的 MFSetAttributeSize/Ratio 内联函数）。
    fn pack2(hi: u32, lo: u32) -> u64 {
        ((hi as u64) << 32) | lo as u64
    }

    fn enum_encoders(flags: MFT_ENUM_FLAG) -> Vec<IMFActivate> {
        unsafe {
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let output = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            let hr = MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(&input),
                Some(&output),
                &mut arr,
                &mut count,
            );
            if hr.is_err() || arr.is_null() || count == 0 {
                if !arr.is_null() {
                    CoTaskMemFree(Some(arr as *const c_void));
                }
                return Vec::new();
            }
            let items = std::slice::from_raw_parts_mut(arr, count as usize);
            let mut out = Vec::new();
            for it in items.iter_mut() {
                if let Some(a) = it.take() {
                    out.push(a);
                }
            }
            CoTaskMemFree(Some(arr as *const c_void));
            out
        }
    }

    /// 逐级尝试：硬件同步 → 任意同步 → 兜底；激活或配置失败都换下一个。
    fn activate_encoder(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> EncoderResult<IMFTransform> {
        let mut last = String::from("系统中没有可用的 H.264 编码器（N 版需安装媒体功能包）");
        let tiers: [(&str, MFT_ENUM_FLAG); 3] = [
            (
                "硬件",
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            ),
            ("软件", MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER),
            ("通用", MFT_ENUM_FLAG_SORTANDFILTER),
        ];
        for (label, flags) in tiers {
            for activate in enum_encoders(flags) {
                let transform: IMFTransform = match unsafe { activate.ActivateObject() } {
                    Ok(t) => t,
                    Err(e) => {
                        last = format!("激活{label}编码器失败: {e}");
                        continue;
                    }
                };
                if let Ok(api) = transform.cast::<ICodecAPI>() {
                    unsafe {
                        let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));
                        let _ = api.SetValue(&CODECAPI_AVEncCommonRealTime, &variant_bool(true));
                        let _ = api.SetValue(
                            &CODECAPI_AVEncCommonRateControlMode,
                            &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
                        );
                        let _ =
                            api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate));
                        let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &variant_u32(fps * 2));
                        let _ =
                            api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &variant_u32(0));
                        let _ = api.SetValue(&CODECAPI_AVEncVideoMaxNumRefFrame, &variant_u32(1));
                    }
                }
                match unsafe { configure(&transform, width, height, fps, bitrate) } {
                    Ok(()) => {
                        unsafe {
                            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
                        }
                        applog!("[peer] H.264 编码器: {label} MFT（{width}x{height}）");
                        return Ok(transform);
                    }
                    Err(e) => {
                        last = format!("配置{label}编码器失败: {e}");
                    }
                }
            }
        }
        Err(last)
    }

    pub struct H264Encoder {
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        transform: IMFTransform,
        codec: Option<ICodecAPI>,
        frame_index: u64,
        nv12: Vec<u8>,
        out_sample: Option<IMFSample>,
        out_buf: Option<IMFMediaBuffer>,
        out_size: u32,
        /// 最近一次见到的 SPS/PPS（Annex B，含起始码），用于 IDR 前补发
        param_sets: Vec<u8>,
    }

    impl H264Encoder {
        pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> EncoderResult<Self> {
            ensure_mf()?;
            // NV12 要求偶数宽高
            let width = (width & !1).max(2);
            let height = (height & !1).max(2);
            let fps = fps.clamp(5, 60);
            let bitrate = bitrate.clamp(200_000, 30_000_000);
            let transform = activate_encoder(width, height, fps, bitrate)?;

            let codec = transform.cast::<ICodecAPI>().ok();

            Ok(Self {
                width,
                height,
                fps,
                bitrate,
                transform,
                codec,
                frame_index: 0,
                nv12: vec![0u8; (width * height * 3 / 2) as usize],
                out_sample: None,
                out_buf: None,
                out_size: 0,
                param_sets: Vec::new(),
            })
        }

        pub fn width(&self) -> u32 {
            self.width
        }

        pub fn height(&self) -> u32 {
            self.height
        }

        pub fn fps(&self) -> u32 {
            self.fps
        }

        pub fn bitrate(&self) -> u32 {
            self.bitrate
        }

        /// 运行中调整码率（CBR）。
        pub fn set_bitrate(&mut self, bitrate: u32) {
            let bitrate = bitrate.clamp(200_000, 30_000_000);
            if bitrate == self.bitrate {
                return;
            }
            self.bitrate = bitrate;
            if let Some(api) = &self.codec {
                unsafe {
                    let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate));
                }
            }
        }

        /// 请求下一帧为 IDR（WebCodecs 解码器重新同步用）。
        pub fn request_keyframe(&mut self) {
            if let Some(api) = &self.codec {
                unsafe {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
                }
            }
        }

        /// 编码一帧 BGRA（stride 为 DXGI RowPitch；可缩放）。
        pub fn encode(
            &mut self,
            bgra: &[u8],
            stride: u32,
            src_w: u32,
            src_h: u32,
            force_key: bool,
        ) -> EncoderResult<Vec<EncodedFrame>> {
            if src_w < 2 || src_h < 2 {
                return Ok(Vec::new());
            }
            bgra_to_nv12(
                bgra,
                stride,
                src_w,
                src_h,
                self.width,
                self.height,
                &mut self.nv12,
            );
            if force_key {
                self.request_keyframe();
            }
            let sample = self.make_input_sample()?;
            unsafe {
                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        // 先排空旧输出，再重试一次
                        let mut out = self.drain()?;
                        self.transform
                            .ProcessInput(0, &sample, 0)
                            .map_err(|e| format!("编码器接收帧失败: {e}"))?;
                        out.extend(self.drain()?);
                        return Ok(out);
                    }
                    Err(e) => return Err(format!("编码器接收帧失败: {e}")),
                }
            }
            self.drain()
        }

        fn make_input_sample(&mut self) -> EncoderResult<IMFSample> {
            let len = self.nv12.len() as u32;
            unsafe {
                let buffer =
                    MFCreateMemoryBuffer(len).map_err(|e| format!("创建输入缓冲失败: {e}"))?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                buffer
                    .Lock(&mut ptr, None, None)
                    .map_err(|e| format!("锁定输入缓冲失败: {e}"))?;
                if ptr.is_null() {
                    return Err("输入缓冲指针为空".to_string());
                }
                std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, len as usize);
                buffer
                    .Unlock()
                    .map_err(|e| format!("解锁输入缓冲失败: {e}"))?;
                buffer
                    .SetCurrentLength(len)
                    .map_err(|e| format!("设置输入长度失败: {e}"))?;
                let sample = MFCreateSample().map_err(|e| format!("创建采样失败: {e}"))?;
                sample
                    .AddBuffer(&buffer)
                    .map_err(|e| format!("挂载输入缓冲失败: {e}"))?;
                let dur = 10_000_000i64 / self.fps.max(1) as i64;
                sample
                    .SetSampleTime(self.frame_index as i64 * dur)
                    .map_err(|e| format!("设置时间戳失败: {e}"))?;
                sample
                    .SetSampleDuration(dur)
                    .map_err(|e| format!("设置时长失败: {e}"))?;
                self.frame_index = self.frame_index.wrapping_add(1);
                Ok(sample)
            }
        }

        fn make_output_sample(&mut self, cb: u32) -> EncoderResult<IMFSample> {
            let need = cb.max(self.width * self.height * 3 / 2).max(256 * 1024);
            unsafe {
                if self.out_sample.is_none() || need > self.out_size {
                    let buffer =
                        MFCreateMemoryBuffer(need).map_err(|e| format!("创建输出缓冲失败: {e}"))?;
                    let sample = MFCreateSample().map_err(|e| format!("创建输出采样失败: {e}"))?;
                    sample
                        .AddBuffer(&buffer)
                        .map_err(|e| format!("挂载输出缓冲失败: {e}"))?;
                    self.out_buf = Some(buffer);
                    self.out_sample = Some(sample);
                    self.out_size = need;
                }
                Ok(self.out_sample.clone().expect("输出采样刚创建"))
            }
        }

        fn drain(&mut self) -> EncoderResult<Vec<EncodedFrame>> {
            let mut frames = Vec::new();
            for _ in 0..8 {
                let info = unsafe {
                    self.transform
                        .GetOutputStreamInfo(0)
                        .map_err(|e| format!("查询输出流信息失败: {e}"))?
                };
                let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
                let sample = if provides {
                    None
                } else {
                    Some(self.make_output_sample(info.cbSize)?)
                };
                let mut data = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(sample),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                };
                let mut status = 0u32;
                let hr = unsafe {
                    self.transform
                        .ProcessOutput(0, std::slice::from_mut(&mut data), &mut status)
                };
                let out_sample = unsafe { std::mem::ManuallyDrop::take(&mut data.pSample) };
                let out_events = unsafe { std::mem::ManuallyDrop::take(&mut data.pEvents) };
                drop(out_events);
                match hr {
                    Ok(()) => {
                        if let Some(s) = out_sample {
                            if let Some(frame) = self.take_frame(&s)? {
                                frames.push(frame);
                            }
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.renegotiate()?;
                    }
                    Err(e) => return Err(format!("编码器输出失败: {e}")),
                }
            }
            Ok(frames)
        }

        fn renegotiate(&mut self) -> EncoderResult<()> {
            unsafe {
                let ty = self
                    .transform
                    .GetOutputAvailableType(0, 0)
                    .map_err(|e| format!("获取新输出类型失败: {e}"))?;
                self.transform
                    .SetOutputType(0, &ty, 0)
                    .map_err(|e| format!("应用新输出类型失败: {e}"))?;
            }
            Ok(())
        }

        fn take_frame(&mut self, sample: &IMFSample) -> EncoderResult<Option<EncodedFrame>> {
            unsafe {
                let buffer = sample
                    .ConvertToContiguousBuffer()
                    .map_err(|e| format!("读取输出缓冲失败: {e}"))?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut len = 0u32;
                buffer
                    .Lock(&mut ptr, None, Some(&mut len))
                    .map_err(|e| format!("锁定输出缓冲失败: {e}"))?;
                if ptr.is_null() || len == 0 {
                    let _ = buffer.Unlock();
                    return Ok(None);
                }
                let mut data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
                let _ = buffer.Unlock();
                if data.is_empty() {
                    return Ok(None);
                }
                data = ensure_annexb(data);
                let key = is_keyframe(&data);
                // 缓存 SPS/PPS，并保证每个 IDR 前都带一份
                let ps = extract_param_sets(&data);
                if !ps.is_empty() {
                    self.param_sets = ps;
                }
                data = ensure_param_sets(data, &self.param_sets, key);
                let ts = sample
                    .GetSampleTime()
                    .map(|t| (t.max(0) as u64) / 10)
                    .unwrap_or(0);
                Ok(Some(EncodedFrame {
                    data,
                    key,
                    ts_us: ts,
                }))
            }
        }
    }

    unsafe fn configure(
        transform: &IMFTransform,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> EncoderResult<()> {
        let out_type: IMFMediaType =
            MFCreateMediaType().map_err(|e| format!("创建输出类型失败: {e}"))?;
        out_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("设置输出主类型失败: {e}"))?;
        out_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(|e| format!("设置 H.264 子类型失败: {e}"))?;
        out_type
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
            .map_err(|e| format!("设置码率失败: {e}"))?;
        out_type
            .SetUINT64(&MF_MT_FRAME_SIZE, pack2(width, height))
            .map_err(|e| format!("设置输出尺寸失败: {e}"))?;
        out_type
            .SetUINT64(&MF_MT_FRAME_RATE, pack2(fps, 1))
            .map_err(|e| format!("设置输出帧率失败: {e}"))?;
        out_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| format!("设置输出扫描方式失败: {e}"))?;
        // 基线 profile：无 B 帧、低延迟；具体 level 由编码器按分辨率填写。
        let _ = out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32);

        transform
            .SetOutputType(0, &out_type, 0)
            .map_err(|e| format!("设置输出类型失败: {e}"))?;

        let in_type: IMFMediaType =
            MFCreateMediaType().map_err(|e| format!("创建输入类型失败: {e}"))?;
        in_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("设置输入主类型失败: {e}"))?;
        in_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|e| format!("设置 NV12 子类型失败: {e}"))?;
        in_type
            .SetUINT64(&MF_MT_FRAME_SIZE, pack2(width, height))
            .map_err(|e| format!("设置输入尺寸失败: {e}"))?;
        in_type
            .SetUINT64(&MF_MT_FRAME_RATE, pack2(fps, 1))
            .map_err(|e| format!("设置输入帧率失败: {e}"))?;
        in_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| format!("设置输入扫描方式失败: {e}"))?;
        let _ = in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, width);
        transform
            .SetInputType(0, &in_type, 0)
            .map_err(|e| format!("设置输入类型失败: {e}"))?;
        Ok(())
    }

    /// Annex B 访问单元 → NAL 类型列表（跳过起始码）。
    pub(super) fn nal_types(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + 3 < data.len() {
            let start = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                Some(3)
            } else if i + 4 < data.len()
                && data[i] == 0
                && data[i + 1] == 0
                && data[i + 2] == 0
                && data[i + 3] == 1
            {
                Some(4)
            } else {
                None
            };
            match start {
                Some(n) => {
                    let idx = i + n;
                    if idx < data.len() {
                        out.push(data[idx] & 0x1f);
                    }
                    i = idx + 1;
                }
                None => i += 1,
            }
        }
        out
    }

    /// 关键帧 = 含 IDR（type 5）。
    pub(super) fn is_keyframe(data: &[u8]) -> bool {
        nal_types(data).contains(&5)
    }

    /// Annex B NAL 类型列表（含起始码位置与类型；start 指向起始码首字节）。
    fn nal_spans(data: &[u8]) -> Vec<(usize, u8)> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + 3 <= data.len() {
            let sc = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                Some(3)
            } else if i + 4 <= data.len()
                && data[i] == 0
                && data[i + 1] == 0
                && data[i + 2] == 0
                && data[i + 3] == 1
            {
                Some(4)
            } else {
                None
            };
            match sc {
                Some(n) => {
                    let hdr = i + n;
                    if hdr < data.len() {
                        out.push((i, data[hdr] & 0x1f));
                    }
                    i = hdr + 1;
                }
                None => i += 1,
            }
        }
        out
    }

    /// 提取 Annex B 中的 SPS(7)/PPS(8)（保留起始码），用于每个 IDR 前补发。
    pub(super) fn extract_param_sets(data: &[u8]) -> Vec<u8> {
        let spans = nal_spans(data);
        let mut out = Vec::new();
        for (k, (start, ty)) in spans.iter().enumerate() {
            if *ty != 7 && *ty != 8 {
                continue;
            }
            let end = spans.get(k + 1).map(|(s, _)| *s).unwrap_or(data.len());
            if *start < end {
                out.extend_from_slice(&data[*start..end]);
            }
        }
        out
    }

    /// 关键帧缺少 SPS 时，把缓存的 SPS/PPS 拼到前面。
    ///
    /// 部分硬件 MFT 只在流开头输出一次 SPS/PPS；分辨率/缩放切换会重建编码器，
    /// 客户端等关键帧重建解码器时若拿不到 SPS/PPS 就会黑屏或花屏。
    pub(super) fn ensure_param_sets(data: Vec<u8>, param_sets: &[u8], key: bool) -> Vec<u8> {
        if !key || param_sets.is_empty() || nal_types(&data).contains(&7) {
            return data;
        }
        let mut out = Vec::with_capacity(param_sets.len() + data.len());
        out.extend_from_slice(param_sets);
        out.extend_from_slice(&data);
        out
    }

    /// 部分 MFT 输出 AVCC（4 字节长度前缀）：统一转成 Annex B。
    pub(super) fn ensure_annexb(data: Vec<u8>) -> Vec<u8> {
        if data.len() >= 4 && (data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])) {
            return data;
        }
        let mut out = Vec::with_capacity(data.len() + 16);
        let mut i = 0usize;
        while i + 4 <= data.len() {
            let n = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
            i += 4;
            if n == 0 || i + n > data.len() {
                break;
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[i..i + n]);
            i += n;
        }
        if out.is_empty() {
            // 无法识别：原样返回，交给前端/解码器判断
            data
        } else {
            out
        }
    }

    /// BGRA（stride 可大于 width*4）→ NV12，支持缩放到偶数 dst 尺寸。
    ///
    /// 色彩矩阵按分辨率选择：HD 及以上用 BT.709，SD 用 BT.601（limited range）。
    /// WebCodecs 对 HD 默认按 BT.709 解释，固定 BT.601 会偏色。
    pub fn bgra_to_nv12(
        bgra: &[u8],
        stride: u32,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
        out: &mut Vec<u8>,
    ) {
        let dw = (dst_w & !1).max(2) as usize;
        let dh = (dst_h & !1).max(2) as usize;
        let need = dw * dh * 3 / 2;
        if out.len() != need {
            out.resize(need, 0);
        }
        let (y_plane, uv_plane) = out.split_at_mut(dw * dh);
        let sw = src_w.max(1) as usize;
        let sh = src_h.max(1) as usize;
        let stride = stride.max(src_w * 4) as usize;
        let max_x = sw - 1;
        let max_y = sh - 1;

        // 预计算缩放映射：内层不再做除法（原实现 1080p 每帧上百万次除法）
        let x_of: Vec<usize> = (0..dw).map(|x| ((x * sw) / dw).min(max_x)).collect();
        let y_of: Vec<usize> = (0..dh).map(|y| ((y * sh) / dh).min(max_y)).collect();

        // 色彩矩阵按**源画面**分辨率选择，而不是缩放后的编码高度：
        // 1080p 自适应缩到 540p 仍是 HD 内容，若此时切到 BT.601，
        // 查看端按 BT.709 解码会出现偏色。
        let hd = src_h >= 720;
        let (yr, yg, yb) = if hd { (47, 157, 16) } else { (66, 129, 25) };
        let (ur, ug, ub) = if hd { (-26, -87, 112) } else { (-38, -74, 112) };
        let (vr, vg, vb) = if hd {
            (112, -102, -10)
        } else {
            (112, -94, -18)
        };

        let px = |idx: usize| -> (i32, i32, i32) {
            if idx + 2 >= bgra.len() {
                return (0, 0, 0);
            }
            // BGRA
            (bgra[idx + 2] as i32, bgra[idx + 1] as i32, bgra[idx] as i32)
        };

        for y in 0..dh {
            let row = y_of[y] * stride;
            let out_row = &mut y_plane[y * dw..(y + 1) * dw];
            for (x, v) in out_row.iter_mut().enumerate() {
                let (r, g, b) = px(row + x_of[x] * 4);
                *v = (((yr * r + yg * g + yb * b + 128) >> 8) + 16).clamp(0, 255) as u8;
            }
        }
        let cw = dw / 2;
        for by in (0..dh).step_by(2) {
            let row0 = y_of[by] * stride;
            let row1 = y_of[by + 1] * stride;
            for bx in (0..dw).step_by(2) {
                let x0 = x_of[bx];
                let x1 = x_of[bx + 1];
                let (r0, g0, b0) = px(row0 + x0 * 4);
                let (r1, g1, b1) = px(row0 + x1 * 4);
                let (r2, g2, b2) = px(row1 + x0 * 4);
                let (r3, g3, b3) = px(row1 + x1 * 4);
                let r = r0 + r1 + r2 + r3;
                let g = g0 + g1 + g2 + g3;
                let b = b0 + b1 + b2 + b3;
                let u = (((ur * r + ug * g + ub * b) >> 8) + 512) / 4;
                let v = (((vr * r + vg * g + vb * b) >> 8) + 512) / 4;
                let ci = (by / 2) * cw + (bx / 2);
                // NV12 的 UV 是交错存放（U,V,U,V…），不是 I420 的两个独立平面；
                // 写成平面布局会让解码端色度整片错位（画面紫/绿块）。
                uv_plane[ci * 2] = u.clamp(0, 255) as u8;
                uv_plane[ci * 2 + 1] = v.clamp(0, 255) as u8;
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub struct H264Encoder;

    impl H264Encoder {
        pub fn new(_w: u32, _h: u32, _fps: u32, _br: u32) -> EncoderResult<Self> {
            Err("H.264 编码仅支持 Windows".to_string())
        }
        pub fn width(&self) -> u32 {
            0
        }
        pub fn height(&self) -> u32 {
            0
        }
        pub fn fps(&self) -> u32 {
            0
        }
        pub fn bitrate(&self) -> u32 {
            0
        }
        pub fn set_bitrate(&mut self, _bitrate: u32) {}
        pub fn request_keyframe(&mut self) {}
        pub fn bgra_to_nv12(
            _b: &[u8],
            _s: u32,
            _sw: u32,
            _sh: u32,
            dw: u32,
            dh: u32,
            out: &mut Vec<u8>,
        ) {
            out.resize((dw * dh * 3 / 2) as usize, 0);
        }

        pub fn encode(
            &mut self,
            _bgra: &[u8],
            _stride: u32,
            _sw: u32,
            _sh: u32,
            _force_key: bool,
        ) -> EncoderResult<Vec<EncodedFrame>> {
            Err("H.264 编码仅支持 Windows".to_string())
        }
    }
}

pub use imp::{bgra_to_nv12, H264Encoder};

/// 编码器是否可用（用于日志/自检；失败不致命）。
pub fn available() -> bool {
    H264Encoder::new(320, 240, 15, 500_000).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn bgra_to_nv12_scales_and_splits_planes() {
        let src_w = 4u32;
        let src_h = 4u32;
        let mut bgra = vec![0u8; (src_w * src_h * 4) as usize];
        for i in 0..(src_w * src_h) as usize {
            // 纯蓝：B=255
            bgra[i * 4] = 255;
            bgra[i * 4 + 3] = 255;
        }
        let mut out = Vec::new();
        bgra_to_nv12(&bgra, src_w * 4, src_w, src_h, 2, 2, &mut out);
        assert_eq!(out.len(), 2 * 2 + 2 * 2 / 2);
        // 蓝色 BT.601 limited：Y≈41，U≈240，V≈110
        assert!((out[0] as i32 - 41).abs() <= 2, "Y={}", out[0]);
        assert!((out[4] as i32 - 240).abs() <= 3, "U={}", out[4]);
        assert!((out[5] as i32 - 110).abs() <= 3, "V={}", out[5]);
    }

    #[cfg(windows)]
    #[test]
    fn nv12_uses_bt709_for_hd() {
        // 720p 起按 BT.709（纯蓝 Y≈32、V≈118；BT.601 是 Y≈41、V≈110）
        let (w, h) = (2u32, 720u32);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for i in 0..(w * h) as usize {
            bgra[i * 4] = 255; // B
            bgra[i * 4 + 3] = 255;
        }
        let mut out = Vec::new();
        bgra_to_nv12(&bgra, w * 4, w, h, w, h, &mut out);
        assert!((out[0] as i32 - 32).abs() <= 2, "Y(BT.709)={}", out[0]);
        let uv = (w * h) as usize;
        assert!(
            (out[uv + 1] as i32 - 118).abs() <= 3,
            "V(BT.709)={}",
            out[uv + 1]
        );
    }

    #[cfg(windows)]
    #[test]
    fn keyframe_prepends_cached_param_sets() {
        let sps = [0u8, 0, 0, 1, 0x67, 0x42];
        let pps = [0u8, 0, 0, 1, 0x68, 0xCE];
        let idr = vec![0u8, 0, 0, 1, 0x65, 0x88];
        let mut full = sps.to_vec();
        full.extend_from_slice(&pps);
        full.extend_from_slice(&idr);

        let ps = imp::extract_param_sets(&full);
        assert_eq!(ps, [sps.as_slice(), pps.as_slice()].concat());

        let out = imp::ensure_param_sets(idr.clone(), &ps, true);
        assert!(out.starts_with(&sps), "IDR 前应补 SPS");
        assert_eq!(&out[ps.len()..], idr.as_slice());

        // 已含 SPS 的关键帧不重复插入；非关键帧不插入
        assert_eq!(imp::ensure_param_sets(full.clone(), &ps, true), full);
        assert_eq!(imp::ensure_param_sets(idr.clone(), &ps, false), idr);
    }

    #[cfg(windows)]
    #[test]
    fn bgra_to_nv12_interleaves_uv() {
        // 4x2：左半纯蓝、右半纯红。NV12 的 UV 必须交错（U,V,U,V）；
        // 若误写成 I420 平面布局（U,U,V,V），色度会整片错位。
        let src_w = 4u32;
        let src_h = 2u32;
        let mut bgra = vec![0u8; (src_w * src_h * 4) as usize];
        for y in 0..src_h as usize {
            for x in 0..src_w as usize {
                let i = (y * src_w as usize + x) * 4;
                if x < 2 {
                    bgra[i] = 255; // B
                } else {
                    bgra[i + 2] = 255; // R
                }
                bgra[i + 3] = 255;
            }
        }
        let mut out = Vec::new();
        bgra_to_nv12(&bgra, src_w * 4, src_w, src_h, src_w, src_h, &mut out);
        assert_eq!(out.len(), (src_w * src_h * 3 / 2) as usize);
        let uv = (src_w * src_h) as usize;
        // 蓝：U≈240 / V≈110；红：U≈90 / V≈240（BT.601 limited）
        assert!((out[uv] as i32 - 240).abs() <= 3, "U(blue)={}", out[uv]);
        assert!(
            (out[uv + 1] as i32 - 110).abs() <= 3,
            "V(blue)={}",
            out[uv + 1]
        );
        assert!(
            (out[uv + 2] as i32 - 90).abs() <= 3,
            "U(red)={}",
            out[uv + 2]
        );
        assert!(
            (out[uv + 3] as i32 - 240).abs() <= 3,
            "V(red)={}",
            out[uv + 3]
        );
    }

    #[cfg(windows)]
    #[test]
    fn annexb_detection_and_avcc_conversion() {
        // 00 00 00 01 67 (SPS) + 00 00 01 65 (IDR)
        let mut stream = vec![0u8, 0, 0, 1, 0x67, 0, 0, 1, 0x65, 0xAA];
        assert_eq!(imp::nal_types(&stream), vec![7, 5]);
        assert!(imp::is_keyframe(&stream));

        // AVCC：len=2, 0x65 0xAA；len=1, 0x41
        let avcc = vec![0u8, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 1, 0x41];
        stream = imp::ensure_annexb(avcc);
        assert!(stream.starts_with(&[0, 0, 0, 1]));
        assert_eq!(imp::nal_types(&stream), vec![5, 1]);
    }

    #[cfg(windows)]
    #[test]
    fn encoder_smoke_if_available() {
        let mut enc = match H264Encoder::new(320, 240, 15, 600_000) {
            Ok(e) => e,
            Err(_) => return, // 无媒体功能包：跳过（调用方会回退 JPEG）
        };
        let frame = vec![128u8; 320 * 240 * 4];
        let mut frames = Vec::new();
        for _ in 0..5 {
            frames.extend(
                enc.encode(&frame, 320 * 4, 320, 240, false)
                    .expect("encode"),
            );
        }
        frames.extend(
            enc.encode(&frame, 320 * 4, 320, 240, true)
                .expect("keyframe"),
        );
        assert!(!frames.is_empty(), "MFT 应至少输出一帧");
        assert!(frames.iter().any(|f| f.key), "强制关键帧应产生 IDR");
        assert!(frames.iter().all(|f| !f.data.is_empty()));
    }
}
