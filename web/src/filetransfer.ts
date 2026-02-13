import type { InputEvent } from "./connection";

const CHUNK_SIZE = 16 * 1024; // 16 KB per chunk

type ProgressCallback = (filename: string, percent: number) => void;

/**
 * Uploads files to the remote desktop via WebRTC DataChannel.
 * Files are chunked, base64-encoded, and sent as FileStart/FileChunk/FileDone events.
 */
export class FileUploader {
  private sendFn: (event: InputEvent) => void;
  private onProgress: ProgressCallback | null = null;

  constructor(sendFn: (event: InputEvent) => void) {
    this.sendFn = sendFn;
  }

  /** Register a progress callback: (filename, percent 0-100) */
  setProgressCallback(cb: ProgressCallback): void {
    this.onProgress = cb;
  }

  /** Upload a file by chunking it and sending over the data channel */
  async uploadFile(file: File): Promise<void> {
    const id = generateTransferId();
    const name = file.name;
    const size = file.size;

    // Send FileStart
    this.sendFn({ t: "fs", id, name, size } as InputEvent);

    let offset = 0;
    while (offset < size) {
      const end = Math.min(offset + CHUNK_SIZE, size);
      const slice = file.slice(offset, end);
      const buffer = await slice.arrayBuffer();
      const data = arrayBufferToBase64(buffer);

      this.sendFn({ t: "fc", id, data } as InputEvent);

      offset = end;
      const percent = Math.round((offset / size) * 100);
      this.onProgress?.(name, percent);

      // Yield to avoid blocking the UI thread on large files
      if (offset < size) {
        await sleep(0);
      }
    }

    // Send FileDone
    this.sendFn({ t: "fd", id } as InputEvent);
    this.onProgress?.(name, 100);
  }
}

function generateTransferId(): string {
  if (typeof crypto !== "undefined" && crypto.randomUUID) {
    return crypto.randomUUID();
  }
  // Fallback: simple random hex string
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

function arrayBufferToBase64(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer);
  let binary = "";
  for (let i = 0; i < bytes.length; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return btoa(binary);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
