//! H.264 编码器 spike：验证本机 MFT 在常见分辨率下的激活/编码/关键帧。
//! 用法：`cargo run --release --example h264_spike -- [width height]`
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sizes: Vec<(u32, u32)> = if args.len() >= 3 {
        vec![(
            args[1].parse().unwrap_or(1920),
            args[2].parse().unwrap_or(1080),
        )]
    } else {
        vec![(1280, 720), (1920, 1080), (2560, 1440), (2560, 1600)]
    };
    for (w, h) in sizes {
        let t0 = std::time::Instant::now();
        match ruthenium_lib::peer::h264::H264Encoder::new(w, h, 30, 6_000_000) {
            Ok(mut enc) => {
                let frame = vec![96u8; (w * h * 4) as usize];
                let mut total = 0usize;
                let mut frames = 0usize;
                let mut keys = 0usize;
                let mut err = String::new();
                for i in 0..60 {
                    match enc.encode(&frame, w * 4, w, h, i == 0) {
                        Ok(out) => {
                            for f in out {
                                total += f.data.len();
                                frames += 1;
                                if f.key {
                                    keys += 1;
                                }
                            }
                        }
                        Err(e) => {
                            err = e;
                            break;
                        }
                    }
                }
                let elapsed = t0.elapsed();
                println!(
                    "{w}x{h}: frames={frames} keys={keys} bytes={total} elapsed={elapsed:?} ({:.1}fps) {}",
                    frames as f64 / elapsed.as_secs_f64().max(0.001),
                    if err.is_empty() { String::new() } else { format!("err={err}") }
                );
            }
            Err(e) => println!("{w}x{h}: unavailable: {e}"),
        }
    }
}
