import type { InputEvent } from "./connection";

const CHUNK_SIZE = 16 * 1024; // 16 KB per chunk

type ProgressCallback = (filename: string, percent: number) => void;

/**
 * Uploads files to the remote desktop via WebSocket.
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

/** Messages sent from the agent for file downloads */
interface DownloadStart {
  t: "fds";
  id: string;
  name: string;
  size: number;
}

interface DownloadChunk {
  t: "fdc";
  id: string;
  data: string;
}

interface DownloadDone {
  t: "fdd";
  id: string;
}

interface DownloadError {
  t: "fde";
  id: string;
  error: string;
}

export type DownloadMessage = DownloadStart | DownloadChunk | DownloadDone | DownloadError;

type DownloadCompleteCallback = (filename: string) => void;
type DownloadErrorCallback = (error: string) => void;

interface ActiveDownload {
  name: string;
  size: number;
  received: number;
  chunks: Uint8Array[];
}

/**
 * Receives file downloads from the remote desktop via WebSocket.
 * Accumulates base64-encoded chunks and triggers a browser download on completion.
 */
export class FileDownloader {
  private downloads = new Map<string, ActiveDownload>();
  private onComplete: DownloadCompleteCallback | null = null;
  private onError: DownloadErrorCallback | null = null;

  setCompleteCallback(cb: DownloadCompleteCallback): void {
    this.onComplete = cb;
  }

  setErrorCallback(cb: DownloadErrorCallback): void {
    this.onError = cb;
  }

  /** Handle an incoming download message from the agent */
  handleMessage(msg: DownloadMessage): void {
    switch (msg.t) {
      case "fds":
        this.downloads.set(msg.id, {
          name: msg.name,
          size: msg.size,
          received: 0,
          chunks: [],
        });
        break;

      case "fdc": {
        const dl = this.downloads.get(msg.id);
        if (!dl) break;
        const decoded = base64ToUint8Array(msg.data);
        dl.chunks.push(decoded);
        dl.received += decoded.length;
        break;
      }

      case "fdd": {
        const dl = this.downloads.get(msg.id);
        if (!dl) break;
        this.downloads.delete(msg.id);
        this.triggerBrowserDownload(dl.name, dl.chunks);
        this.onComplete?.(dl.name);
        break;
      }

      case "fde":
        this.downloads.delete(msg.id);
        this.onError?.(msg.error);
        break;
    }
  }

  private triggerBrowserDownload(filename: string, chunks: Uint8Array[]): void {
    const blob = new Blob(chunks as BlobPart[], { type: "application/octet-stream" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = filename;
    link.style.display = "none";
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
    // Revoke after a short delay to ensure the download starts
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
  }
}

function base64ToUint8Array(b64: string): Uint8Array {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}
