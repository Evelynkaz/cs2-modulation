// Small inline SVG icons (S6m redesign - offline app, no icon font/CDN). Every icon is a 24x24
// viewBox, stroke=currentColor so it follows the surrounding text/accent color; callers set the
// pixel size via the `.icon` CSS class (width/height). `icon(name)` returns markup for
// `el(...).innerHTML`, not a live node - cheap to stamp into many rows (lineup cards, chips).

const PATHS = {
  chevron: '<path d="M6 9l6 6 6-6"/>',
  close: '<path d="M6 6l12 12M18 6L6 18"/>',
  target: '<circle cx="12" cy="12" r="7"/><circle cx="12" cy="12" r="2.5"/>',
  corner: '<path d="M5 19V5h14"/><path d="M5 19l6-6"/>',
  wall: '<path d="M4 6h16M4 12h16M4 18h16"/>',
  standFigure: '<circle cx="12" cy="5" r="2.2"/><path d="M12 8v8M8 11h8M9 21l3-5 3 5"/>',
  crouchFigure: '<circle cx="12" cy="8" r="2"/><path d="M12 10.5v4M8 12h8M8 20v-4l4-2 4 2v4"/>',
  jumpFigure: '<circle cx="12" cy="4.5" r="2"/><path d="M12 7v5M9 8.5l3 1.5 3-1.5M8 20l4-8 4 8"/>',
  crouchJumpFigure: '<circle cx="12" cy="6" r="1.9"/><path d="M12 8.5v3.5M9 9.5l3 1 3-1M8 19l4-6.5 4 6.5"/>',
  clock: '<circle cx="12" cy="12" r="8"/><path d="M12 7v5l3.5 2"/>',
  bounce: '<circle cx="6" cy="18" r="1.6"/><circle cx="13" cy="13" r="1.6"/><circle cx="19" cy="18" r="1.6"/><path d="M6 18C8 8 16 8 19 18" fill="none"/>',
  gauge: '<path d="M4 16a8 8 0 0 1 16 0"/><path d="M12 16l4-5"/><circle cx="12" cy="16" r="1.2"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="1.5"/><path d="M5 15V5a1 1 0 0 1 1-1h10"/>',
  eye: '<path d="M2 12s3.6-6.5 10-6.5S22 12 22 12s-3.6 6.5-10 6.5S2 12 2 12z"/><circle cx="12" cy="12" r="2.5"/>',
  camera: '<rect x="3" y="7" width="15" height="12" rx="2"/><path d="M8 7l1.6-2.5h4.8L16 7"/><circle cx="10.5" cy="13" r="3"/><path d="M18 10.5l3-1.7v7.4l-3-1.7"/>',
  sun: '<circle cx="12" cy="12" r="4.2"/><path d="M12 2.5v2.4M12 19.1v2.4M4.6 4.6l1.7 1.7M17.7 17.7l1.7 1.7M2.5 12h2.4M19.1 12h2.4M4.6 19.4l1.7-1.7M17.7 6.3l1.7-1.7"/>',
  moon: '<path d="M20 14.5A8.5 8.5 0 1 1 9.5 4a7 7 0 0 0 10.5 10.5z"/>',
  gear: '<circle cx="12" cy="12" r="3"/><path d="M12 3v2.2M12 18.8V21M21 12h-2.2M5.2 12H3M18.4 5.6l-1.6 1.6M7.2 16.8l-1.6 1.6M18.4 18.4l-1.6-1.6M7.2 7.2 5.6 5.6"/>',
  warning: '<path d="M12 3.5 22 20.5H2z"/><path d="M12 10v4.2"/><circle cx="12" cy="17.2" r="0.9"/>',
  check: '<path d="M4 12.5l5 5 11-11"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  layers: '<path d="M12 3l9 5-9 5-9-5 9-5z"/><path d="M3 13l9 5 9-5"/>',
};

export function icon(name, size = 16) {
  const body = PATHS[name] ?? "";
  return `<svg class="icon" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
}

export const THROW_TYPE_ICON = {
  Stand: "standFigure",
  Crouch: "crouchFigure",
  JumpThrow: "jumpFigure",
  CrouchJumpThrow: "crouchJumpFigure",
};
