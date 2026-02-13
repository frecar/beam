import type { InputEvent } from "./connection";

/**
 * Bridges the browser clipboard with the remote desktop.
 * - On paste: reads local clipboard and sends text to remote
 * - On remote clipboard event: writes to local clipboard
 */
export class ClipboardBridge {
  private sendClipboard: (event: InputEvent) => void;
  private onPaste = this.handlePaste.bind(this);

  constructor(sendClipboard: (event: InputEvent) => void) {
    this.sendClipboard = sendClipboard;
  }

  enable(): void {
    document.addEventListener("paste", this.onPaste);
  }

  disable(): void {
    document.removeEventListener("paste", this.onPaste);
  }

  /** Called when the remote sends clipboard text */
  handleRemoteClipboard(text: string): void {
    navigator.clipboard.writeText(text).catch(() => {
      // Clipboard write permission denied — ignore silently
    });
  }

  private handlePaste(e: ClipboardEvent): void {
    const MAX_CLIPBOARD_BYTES = 1_048_576; // 1 MB
    const text = e.clipboardData?.getData("text");
    if (text && text.length <= MAX_CLIPBOARD_BYTES) {
      e.preventDefault();
      this.sendClipboard({ t: "c", text });
    }
  }
}
