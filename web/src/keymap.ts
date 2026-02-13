/**
 * Maps browser KeyboardEvent.code values to Linux evdev key codes.
 * Reference: linux/input-event-codes.h
 */

const KEYMAP: Record<string, number> = {
  // Row 1: Escape + Function keys
  Escape: 1,
  F1: 59,
  F2: 60,
  F3: 61,
  F4: 62,
  F5: 63,
  F6: 64,
  F7: 65,
  F8: 66,
  F9: 67,
  F10: 68,
  F11: 87,
  F12: 88,

  // Row 2: Backtick + Digits + Backspace
  Backquote: 41,
  Digit1: 2,
  Digit2: 3,
  Digit3: 4,
  Digit4: 5,
  Digit5: 6,
  Digit6: 7,
  Digit7: 8,
  Digit8: 9,
  Digit9: 10,
  Digit0: 11,
  Minus: 12,
  Equal: 13,
  Backspace: 14,

  // Row 3: Tab + Letters + Bracket/Backslash
  Tab: 15,
  KeyQ: 16,
  KeyW: 17,
  KeyE: 18,
  KeyR: 19,
  KeyT: 20,
  KeyY: 21,
  KeyU: 22,
  KeyI: 23,
  KeyO: 24,
  KeyP: 25,
  BracketLeft: 26,
  BracketRight: 27,
  Backslash: 43,

  // Row 4: CapsLock + Letters + Enter
  CapsLock: 58,
  KeyA: 30,
  KeyS: 31,
  KeyD: 32,
  KeyF: 33,
  KeyG: 34,
  KeyH: 35,
  KeyJ: 36,
  KeyK: 37,
  KeyL: 38,
  Semicolon: 39,
  Quote: 40,
  Enter: 28,

  // Row 5: Shift + Letters + Shift
  ShiftLeft: 42,
  IntlBackslash: 86,
  KeyZ: 44,
  KeyX: 45,
  KeyC: 46,
  KeyV: 47,
  KeyB: 48,
  KeyN: 49,
  KeyM: 50,
  Comma: 51,
  Period: 52,
  Slash: 53,
  ShiftRight: 54,

  // Row 6: Bottom row
  ControlLeft: 29,
  MetaLeft: 125,
  AltLeft: 56,
  Space: 57,
  AltRight: 100,
  MetaRight: 126,
  ContextMenu: 127,
  ControlRight: 97,

  // Navigation cluster
  PrintScreen: 99,
  ScrollLock: 70,
  Pause: 119,
  Insert: 110,
  Home: 102,
  PageUp: 104,
  Delete: 111,
  End: 107,
  PageDown: 109,

  // Arrow keys
  ArrowUp: 103,
  ArrowLeft: 105,
  ArrowDown: 108,
  ArrowRight: 106,

  // Numpad
  NumLock: 69,
  NumpadDivide: 98,
  NumpadMultiply: 55,
  NumpadSubtract: 74,
  Numpad7: 71,
  Numpad8: 72,
  Numpad9: 73,
  NumpadAdd: 78,
  Numpad4: 75,
  Numpad5: 76,
  Numpad6: 77,
  Numpad1: 79,
  Numpad2: 80,
  Numpad3: 81,
  NumpadEnter: 96,
  Numpad0: 82,
  NumpadDecimal: 83,
};

export function keyCodeToEvdev(code: string): number | undefined {
  return KEYMAP[code];
}
