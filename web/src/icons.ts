/**
 * Inline SVG icon constants for status bar buttons.
 * Stroke-based, 14x14, currentColor for theme compatibility.
 */

const s = (d: string) =>
  `<svg aria-hidden="true" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${d}</svg>`;

export const ICON_UPLOAD = s(
  '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="17 8 12 3 7 8"/><line x1="12" y1="3" x2="12" y2="15"/>'
);

export const ICON_DOWNLOAD = s(
  '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/>'
);

export const ICON_CAPTURE = s(
  '<rect x="2" y="4" width="20" height="16" rx="2" ry="2"/><line x1="6" y1="8" x2="6.01" y2="8"/><line x1="10" y1="8" x2="10.01" y2="8"/><line x1="14" y1="8" x2="14.01" y2="8"/><line x1="18" y1="8" x2="18.01" y2="8"/><line x1="8" y1="12" x2="8.01" y2="12"/><line x1="12" y1="12" x2="12.01" y2="12"/><line x1="16" y1="12" x2="16.01" y2="12"/><line x1="7" y1="16" x2="17" y2="16"/>'
);

export const ICON_UNMUTE = s(
  '<polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"/><line x1="23" y1="9" x2="17" y2="15"/><line x1="17" y1="9" x2="23" y2="15"/>'
);

export const ICON_MUTE = s(
  '<polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"/><path d="M19.07 4.93a10 10 0 0 1 0 14.14"/><path d="M15.54 8.46a5 5 0 0 1 0 7.07"/>'
);

export const ICON_FULLSCREEN = s(
  '<polyline points="15 3 21 3 21 9"/><polyline points="9 21 3 21 3 15"/><line x1="21" y1="3" x2="14" y2="10"/><line x1="3" y1="21" x2="10" y2="14"/>'
);

export const ICON_SUN = s(
  '<circle cx="12" cy="12" r="5"/><line x1="12" y1="1" x2="12" y2="3"/><line x1="12" y1="21" x2="12" y2="23"/><line x1="4.22" y1="4.22" x2="5.64" y2="5.64"/><line x1="18.36" y1="18.36" x2="19.78" y2="19.78"/><line x1="1" y1="12" x2="3" y2="12"/><line x1="21" y1="12" x2="23" y2="12"/><line x1="4.22" y1="19.78" x2="5.64" y2="18.36"/><line x1="18.36" y1="5.64" x2="19.78" y2="4.22"/>'
);

export const ICON_MOON = s('<path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>');

export const ICON_DISCONNECT = s(
  '<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" y1="12" x2="9" y2="12"/>'
);

export const ICON_POWER = s(
  '<path d="M18.36 6.64a9 9 0 1 1-12.73 0"/><line x1="12" y1="2" x2="12" y2="12"/>'
);

// Keyboard icon — used on the FAB toggle to "Show keyboard" action.
// 24-key layout matches the on-screen keyboard convention.
export const ICON_KEYBOARD = s(
  '<rect x="2" y="6" width="20" height="12" rx="2" ry="2"/><line x1="6" y1="10" x2="6.01" y2="10"/><line x1="10" y1="10" x2="10.01" y2="10"/><line x1="14" y1="10" x2="14.01" y2="10"/><line x1="18" y1="10" x2="18.01" y2="10"/><line x1="7" y1="14" x2="17" y2="14"/>'
);

// Keyboard-off icon — used on the "Hide keyboard" FAB action introduced
// for G2 P7. Same as ICON_KEYBOARD with a diagonal slash overlay.
export const ICON_KEYBOARD_OFF = s(
  '<rect x="2" y="6" width="20" height="12" rx="2" ry="2"/><line x1="7" y1="14" x2="17" y2="14"/><line x1="2" y1="2" x2="22" y2="22"/>'
);

// Image-capture / screenshot icon.
export const ICON_SCREENSHOT = s(
  '<path d="M23 19a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4l2-3h6l2 3h4a2 2 0 0 1 2 2z"/><circle cx="12" cy="13" r="4"/>'
);

// Globe / language icon for the FAB keyboard-layout select row.
export const ICON_LAYOUT = s(
  '<circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>'
);

// Vertical-scroll icon for the FAB scroll-speed select row.
export const ICON_SCROLL = s(
  '<polyline points="6 9 12 3 18 9"/><polyline points="6 15 12 21 18 15"/>'
);

// Help icon (circle + question mark) — used on the FAB "Help" action
// introduced for G9 (#101) so touch users can reach the help overlay
// without an F-key.
export const ICON_HELP = s(
  '<circle cx="12" cy="12" r="10"/><path d="M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3"/><line x1="12" y1="17" x2="12.01" y2="17"/>'
);

// Stats / activity icon — used on the FAB "Stats" action introduced
// for G9 (#101) so touch users can toggle the performance overlay
// without F9.
export const ICON_STATS = s('<polyline points="22 12 18 12 15 21 9 3 6 12 2 12"/>');

// Close (X) icon — used on the help-overlay explicit close button
// introduced for G9 (#101). Mobile users cannot press Esc (the input
// layer captures Esc for the remote session), so the modal needs a
// visible close affordance.
export const ICON_CLOSE = s(
  '<line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/>'
);
