import { beforeEach, describe, expect, it, vi } from 'vitest';

import { ClipboardBridge } from './clipboard';
import type { InputEvent } from './connection';

// Mock navigator.clipboard
const mockWriteText = vi.fn<(text: string) => Promise<void>>();
const mockReadText = vi.fn<() => Promise<string>>();

Object.defineProperty(navigator, 'clipboard', {
  value: { writeText: mockWriteText, readText: mockReadText },
  writable: true,
});

describe('ClipboardBridge', () => {
  let sendClipboard: ReturnType<typeof vi.fn<(event: InputEvent) => void>>;
  let bridge: ClipboardBridge;

  beforeEach(() => {
    vi.restoreAllMocks();
    sendClipboard = vi.fn();
    bridge = new ClipboardBridge(sendClipboard);
    mockWriteText.mockResolvedValue(undefined);
    mockReadText.mockResolvedValue('');
  });

  it('handleRemoteClipboard writes to browser clipboard', () => {
    bridge.handleRemoteClipboard('hello world');
    expect(mockWriteText).toHaveBeenCalledWith('hello world');
  });

  it('calls error callback on writeText failure', async () => {
    const errorCb = vi.fn();
    bridge.onClipboardError(errorCb);
    mockWriteText.mockRejectedValue(new DOMException('Not allowed'));

    bridge.handleRemoteClipboard('test');

    // Wait for the promise rejection to propagate
    await vi.waitFor(() => expect(errorCb).toHaveBeenCalledTimes(1));
  });

  it('debounces error notifications', async () => {
    const errorCb = vi.fn();
    bridge.onClipboardError(errorCb);
    mockWriteText.mockRejectedValue(new DOMException('Not allowed'));

    bridge.handleRemoteClipboard('test1');
    await vi.waitFor(() => expect(errorCb).toHaveBeenCalledTimes(1));

    // Second failure within 10s should not trigger another callback
    bridge.handleRemoteClipboard('test2');
    // Give it time to settle
    await new Promise((r) => setTimeout(r, 50));
    expect(errorCb).toHaveBeenCalledTimes(1);
  });

  it('sendPrimaryClipboard sends clipboard to remote', async () => {
    mockReadText.mockResolvedValue('pasted text');
    await bridge.sendPrimaryClipboard();
    expect(sendClipboard).toHaveBeenCalledWith({ t: 'cp', text: 'pasted text' });
  });
});
