// @vitest-environment jsdom
// 配对事件路由：未知/重复/排队终止事件不得顶掉当前确认框。
import { beforeEach, describe, expect, it, vi } from 'vitest';

const apiMock = vi.hoisted(() => ({
  peerDiscover: vi.fn(async () => []),
  peerPairCancel: vi.fn(async () => true),
  peerPairConfirm: vi.fn(async () => true),
  peerPairStart: vi.fn(async () => 'p-new'),
}));

vi.mock('../ipc/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../ipc/commands')>();
  return {
    ...actual,
    peerDiscover: apiMock.peerDiscover,
    peerPairCancel: apiMock.peerPairCancel,
    peerPairConfirm: apiMock.peerPairConfirm,
    peerPairStart: apiMock.peerPairStart,
  };
});

import { useAppStore } from './useAppStore';
import type * as api from '../ipc/commands';

const prompt = (id: string): Extract<api.PairEvent, { status: 'prompt' }> => ({
  status: 'prompt',
  pairingId: id,
  code: '123456',
  peerName: id,
  peerFp: 'fp',
  peerAddr: '10.0.0.2',
  direction: 'incoming',
  numeric: true,
  expiresAtMs: Date.now() + 60_000,
});

const waiting = (id: string): Extract<api.PairEvent, { status: 'waiting' }> => ({
  status: 'waiting',
  pairingId: id,
  peerName: id,
  peerAddr: '10.0.0.3',
  expiresAtMs: Date.now() + 60_000,
});

const grants: api.PeerGrants = {
  files: false, control: false, config: false, screen: false, clipboard: false, audio: false,
};

beforeEach(() => {
  useAppStore.setState({ pair: null, pairQueue: [], pairBusy: false, pendingOutgoing: [], pairWaiting: [], peers: [], discovered: [] });
  apiMock.peerDiscover.mockClear();
  apiMock.peerPairCancel.mockClear();
  apiMock.peerPairConfirm.mockClear();
  apiMock.peerPairStart.mockClear();
});

describe('pairing event routing', () => {
  it('unrelated error must not dismiss the current prompt', () => {
    useAppStore.setState({ pair: prompt('A'), pairQueue: [prompt('B')] });
    useAppStore.getState().onPeerPairing({ status: 'error', pairingId: 'scan-1', detail: 'junk' });
    const s = useAppStore.getState();
    expect(s.pair?.pairingId).toBe('A');
    expect(s.pairQueue.map((p) => p.pairingId)).toEqual(['B']);
  });

  it('duplicate terminal events for A must keep B alive', () => {
    useAppStore.setState({ pair: prompt('A'), pairQueue: [prompt('B')] });
    useAppStore.getState().onPeerPairing({ status: 'timeout', pairingId: 'A' });
    useAppStore.getState().onPeerPairing({ status: 'error', pairingId: 'A', detail: 'late' });
    const s = useAppStore.getState();
    expect(s.pair?.pairingId).toBe('B');
    expect(s.pairQueue).toEqual([]);
  });

  it('timeout of queued B must keep A, not display dead B', () => {
    useAppStore.setState({ pair: prompt('A'), pairQueue: [prompt('B')] });
    useAppStore.getState().onPeerPairing({ status: 'timeout', pairingId: 'B' });
    const s = useAppStore.getState();
    expect(s.pair?.pairingId).toBe('A');
    expect(s.pairQueue).toEqual([]);
  });

  it('duplicate prompt id is ignored', () => {
    useAppStore.setState({ pair: prompt('A') });
    useAppStore.getState().onPeerPairing(prompt('A'));
    expect(useAppStore.getState().pairQueue).toEqual([]);
  });

  it('success always updates peers even if the prompt was dismissed', () => {
    const peer = {
      id: 'p1', name: 'peer', pubkey: 'ssh-ed25519 AAAA', pairedAt: '',
      grantFiles: false, grantControl: false, grantConfig: false,
      grantScreen: false, grantClipboard: false, grantAudio: false,
      peerAllowsFiles: false, peerAllowsControl: false, peerAllowsConfig: false,
      peerAllowsScreen: false, peerAllowsClipboard: false, peerAllowsAudio: false,
    };
    useAppStore.getState().onPeerPairing({ status: 'ok', pairingId: 'lost', peer });
    expect(useAppStore.getState().peers.map((p) => p.id)).toEqual(['p1']);
  });

  it('pairStart tracks the returned outgoing id', async () => {
    await useAppStore.getState().pairStart(undefined, '10.0.0.1');
    expect(useAppStore.getState().pendingOutgoing).toEqual(['p-new']);
  });

  it('pairCancel only settles the current id and advances once', () => {
    useAppStore.setState({ pair: prompt('A'), pairQueue: [prompt('B')] });
    useAppStore.getState().pairCancel();
    const s = useAppStore.getState();
    expect(s.pair?.pairingId).toBe('B');
    expect(s.pairQueue).toEqual([]);
    expect(apiMock.peerPairCancel).toHaveBeenCalledWith('A');
  });

  it('pairConfirm ignores a second click while first IPC is in flight', async () => {
    let resolve: (v: boolean) => void = () => {};
    apiMock.peerPairConfirm.mockImplementationOnce(() => new Promise<boolean>((r) => { resolve = r; }));
    useAppStore.setState({ pair: prompt('A'), pairQueue: [prompt('B')] });
    const first = useAppStore.getState().pairConfirm(true, grants);
    const second = useAppStore.getState().pairConfirm(true, grants);
    expect(apiMock.peerPairConfirm).toHaveBeenCalledTimes(1);
    resolve(false);
    await first;
    await second;
    expect(useAppStore.getState().pair?.pairingId).toBe('B');
  });

  it('accepted decision stays busy until a terminal event', async () => {
    apiMock.peerPairConfirm.mockResolvedValueOnce(true);
    useAppStore.setState({ pair: prompt('A'), pairQueue: [prompt('B')] });
    await useAppStore.getState().pairConfirm(true, grants);
    expect(useAppStore.getState().pairBusy).toBe(true);
    useAppStore.getState().onPeerPairing({ status: 'timeout', pairingId: 'A' });
    expect(useAppStore.getState().pair?.pairingId).toBe('B');
    expect(useAppStore.getState().pairBusy).toBe(false);
  });

  it('waiting event shows a light-weight row and clears pendingOutgoing', async () => {
    useAppStore.setState({ pendingOutgoing: ['W'] });
    useAppStore.getState().onPeerPairing(waiting('W'));
    const s = useAppStore.getState();
    expect(s.pairWaiting.map((w) => w.pairingId)).toEqual(['W']);
    expect(s.pendingOutgoing).toEqual([]);
    // 一键等待不占用确认弹窗队列：后到的入站 prompt 仍能立即显示。
    useAppStore.getState().onPeerPairing(prompt('A'));
    expect(useAppStore.getState().pair?.pairingId).toBe('A');
  });

  it('terminal event for a waiting id removes only that row', async () => {
    useAppStore.setState({ pair: prompt('A'), pairWaiting: [waiting('W')] });
    useAppStore.getState().onPeerPairing({ status: 'rejected', pairingId: 'W', reason: 'user' });
    const s = useAppStore.getState();
    expect(s.pairWaiting).toEqual([]);
    expect(s.pair?.pairingId).toBe('A');
  });

  it('pairCancelWaiting cancels the outgoing attempt without touching the dialog', () => {
    useAppStore.setState({ pair: prompt('A'), pairWaiting: [waiting('W')] });
    useAppStore.getState().pairCancelWaiting('W');
    const s = useAppStore.getState();
    expect(s.pairWaiting).toEqual([]);
    expect(s.pair?.pairingId).toBe('A');
    expect(apiMock.peerPairCancel).toHaveBeenCalledWith('W');
  });

  it('just-works prompt keeps numeric=false so the UI hides the code', () => {
    const oneTap = { ...prompt('J'), numeric: false };
    useAppStore.getState().onPeerPairing(oneTap);
    expect(useAppStore.getState().pair?.numeric).toBe(false);
  });
});
