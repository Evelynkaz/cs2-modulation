// The 2D radar canvas: pan/zoom/recolor (moved here from `main.js`, unchanged), plus everything
// `s6f2_solve_ui.md` adds on top - picking a target/origin, drawing the search's progress points,
// and previewing a selected lineup. One canvas, redrawn as a whole (recolored radar is cached in
// `source`, so a redraw is just a blit); incoming `checked`/`verified` batches only mark the view
// dirty and a `requestAnimationFrame` loop coalesces them into ~one draw per frame.

// R = class (0 floor, 128 low cover, 255 wall), G = floor height tint, A = nav coverage. The
// boundary outline is written as pure red ([255,0,0,255]) - the same bytes a wall pixel with a
// zero height-tint would already have, so no special case is needed: it just paints as "wall".
const PALETTES = {
  light: { floorLo: [214, 217, 222], floorHi: [236, 238, 241], cover: [176, 182, 192], wall: [46, 50, 58] },
  dark: { floorLo: [36, 40, 48], floorHi: [58, 64, 76], cover: [86, 94, 108], wall: [214, 219, 227] },
};

const MARK = {
  light: { target: "#b3261e", origin: "#2563eb", checked: "rgba(91,98,112,0.35)", verified: "#1a7f37", verifiedFail: "rgba(179,38,30,0.35)", selected: "#b3261e", hover: "#2563eb" },
  dark: { target: "#ff6b64", origin: "#5b9dff", checked: "rgba(154,161,173,0.35)", verified: "#4fd17a", verifiedFail: "rgba(255,107,100,0.35)", selected: "#ff6b64", hover: "#5b9dff" },
};

function lerp(a, b, t) {
  return [
    Math.round(a[0] + (b[0] - a[0]) * t),
    Math.round(a[1] + (b[1] - a[1]) * t),
    Math.round(a[2] + (b[2] - a[2]) * t),
  ];
}

function recolorImage(img, theme) {
  const off = document.createElement("canvas");
  off.width = img.naturalWidth;
  off.height = img.naturalHeight;
  const octx = off.getContext("2d");
  octx.drawImage(img, 0, 0);
  const imageData = octx.getImageData(0, 0, off.width, off.height);
  const d = imageData.data;
  const palette = PALETTES[theme] ?? PALETTES.light;
  for (let i = 0; i < d.length; i += 4) {
    const r = d[i];
    const g = d[i + 1];
    const a = d[i + 3];
    if (a === 0) {
      d[i + 3] = 0;
      continue;
    }
    let color;
    if (r >= 192) {
      color = palette.wall;
    } else if (r >= 64) {
      color = palette.cover;
    } else {
      color = lerp(palette.floorLo, palette.floorHi, g / 255);
    }
    d[i] = color[0];
    d[i + 1] = color[1];
    d[i + 2] = color[2];
    d[i + 3] = 255;
  }
  octx.putImageData(imageData, 0, 0);
  return off;
}

// `canvas`: the radar `<canvas>`. `viewerMap`: `/api/radar`'s `{region:[x0,y0,x1,y1], pixelSize}`.
// `img`: the loaded `viewer-map.png`. `theme`: `"light"|"dark"`, for the initial recolor.
export function createMapView(canvas, viewerMap, img, theme) {
  const ctx = canvas.getContext("2d");
  let source = recolorImage(img, theme);
  let userScale = 1;
  const offset = { x: 0, y: 0 };
  const [x0, y0, x1, y1] = viewerMap.region;
  const pixelSize = viewerMap.pixelSize;

  let target = null; // { x, y, z, label }
  let origin = null; // { x, y, reach }
  let lineups = []; // [{ id, feet:[x,y,z], rest:[x,y,z] }]
  let selectedId = null;
  let hoverId = null;
  let checkedPoints = []; // [{x,y}]
  let verifiedPoints = []; // [{x,y}] - `ok === true`
  let verifiedFailPoints = []; // [{x,y}] - `ok === false`

  let onClickHandler = null;
  let onRightClickHandler = null;

  function fitScale() {
    const rect = canvas.parentElement.getBoundingClientRect();
    return Math.min(rect.width / source.width, rect.height / source.height);
  }

  // Image-pixel <-> world, per `s6f2_solve_ui.md`: row 0 is the north edge, so world Y is
  // inverted relative to image rows (`radar/src/lib.rs`'s own `wx = x0 + (px+0.5)*pixelSize`,
  // `wy = y1 - (py+0.5)*pixelSize`).
  function worldToPixel(wx, wy) {
    return [(wx - x0) / pixelSize - 0.5, (y1 - wy) / pixelSize - 0.5];
  }
  function pixelToWorld(ix, iy) {
    return [x0 + (ix + 0.5) * pixelSize, y1 - (iy + 0.5) * pixelSize];
  }

  function screenGeometry() {
    const rect = canvas.getBoundingClientRect();
    const scale = fitScale() * userScale;
    const cx = rect.width / 2 + offset.x;
    const cy = rect.height / 2 + offset.y;
    return { rect, scale, cx, cy };
  }

  // Screen (client, CSS px) -> world. Extrapolates past the canvas edge rather than returning
  // `null` - panning/zooming keeps the math valid everywhere, on-canvas or not.
  function screenToWorld(clientX, clientY) {
    const { rect, scale, cx, cy } = screenGeometry();
    const mx = clientX - rect.left;
    const my = clientY - rect.top;
    const ix = (mx - cx) / scale + source.width / 2;
    const iy = (my - cy) / scale + source.height / 2;
    return pixelToWorld(ix, iy);
  }

  // World -> screen (client, CSS px relative to the canvas).
  function worldToScreen(wx, wy) {
    const { rect, scale, cx, cy } = screenGeometry();
    const [ix, iy] = worldToPixel(wx, wy);
    return [rect.left + cx + (ix - source.width / 2) * scale, rect.top + cy + (iy - source.height / 2) * scale];
  }

  // Same as `worldToScreen`, but relative to the canvas's own top-left (for `ctx` drawing).
  // `geom`, when given, is a precomputed `screenGeometry()` result - `draw()` hoists one per
  // frame instead of paying its `getBoundingClientRect()` calls once per point.
  function worldToCanvas(wx, wy, geom) {
    const { scale, cx, cy } = geom ?? screenGeometry();
    const [ix, iy] = worldToPixel(wx, wy);
    return [cx + (ix - source.width / 2) * scale, cy + (iy - source.height / 2) * scale];
  }

  function marks() {
    return MARK[theme] ?? MARK.light;
  }

  function drawPoints(points, color, radius, geom) {
    if (points.length === 0) {
      return;
    }
    ctx.fillStyle = color;
    for (const p of points) {
      const [sx, sy] = worldToCanvas(p.x, p.y, geom);
      ctx.fillRect(sx - radius, sy - radius, radius * 2, radius * 2);
    }
  }

  function drawCross(sx, sy, color, size = 7) {
    ctx.strokeStyle = color;
    ctx.lineWidth = 2;
    ctx.beginPath();
    ctx.moveTo(sx - size, sy);
    ctx.lineTo(sx + size, sy);
    ctx.moveTo(sx, sy - size);
    ctx.lineTo(sx, sy + size);
    ctx.stroke();
  }

  // Only touches `canvas.width`/`height` (which resets the drawing buffer) when the CSS box or
  // the device pixel ratio actually changed - reassigning them on every `draw()` was itself a
  // chunk of the per-frame cost.
  let lastSizeKey = null;
  function syncCanvasSize(rect, dpr) {
    const key = `${rect.width}x${rect.height}x${dpr}`;
    if (key === lastSizeKey) {
      return;
    }
    lastSizeKey = key;
    canvas.width = Math.max(1, Math.round(rect.width * dpr));
    canvas.height = Math.max(1, Math.round(rect.height * dpr));
    canvas.style.width = `${rect.width}px`;
    canvas.style.height = `${rect.height}px`;
  }

  function draw() {
    const dpr = window.devicePixelRatio || 1;
    const rect = canvas.parentElement.getBoundingClientRect();
    syncCanvasSize(rect, dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, rect.width, rect.height);
    const scale = fitScale() * userScale;
    // The default view downscales the source (e.g. 2056x1752 into ~920x630); nearest-neighbour
    // there aliases thin walls away. Only go nearest when magnifying past 1:1.
    ctx.imageSmoothingEnabled = scale < 1;
    if (scale < 1) {
      ctx.imageSmoothingQuality = "high";
    }
    const dw = source.width * scale;
    const dh = source.height * scale;
    const cx = rect.width / 2 + offset.x;
    const cy = rect.height / 2 + offset.y;
    ctx.drawImage(source, cx - dw / 2, cy - dh / 2, dw, dh);

    // Hoisted once per frame (AMBER-6) - `worldToCanvas`/`drawPoints` then skip their own
    // `getBoundingClientRect()` work per point.
    const geom = { rect, scale, cx, cy };
    const m = marks();
    drawPoints(checkedPoints, m.checked, 1.5, geom);
    drawPoints(verifiedFailPoints, m.verifiedFail, 1.5, geom);
    drawPoints(verifiedPoints, m.verified, 2, geom);

    if (origin) {
      const [sx, sy] = worldToCanvas(origin.x, origin.y, geom);
      ctx.fillStyle = m.origin;
      ctx.beginPath();
      ctx.arc(sx, sy, 4, 0, Math.PI * 2);
      ctx.fill();
      if (origin.reach) {
        ctx.strokeStyle = m.origin;
        ctx.lineWidth = 1;
        ctx.beginPath();
        ctx.arc(sx, sy, (origin.reach / pixelSize) * scale, 0, Math.PI * 2);
        ctx.stroke();
      }
    }

    for (const l of lineups) {
      if (l.id !== selectedId && l.id !== hoverId) {
        continue;
      }
      const [ox, oy] = worldToCanvas(l.feet[0], l.feet[1], geom);
      const [rx, ry] = worldToCanvas(l.rest[0], l.rest[1], geom);
      const color = l.id === selectedId ? m.selected : m.hover;
      ctx.strokeStyle = color;
      ctx.lineWidth = l.id === selectedId ? 2 : 1;
      ctx.setLineDash(l.id === hoverId && l.id !== selectedId ? [4, 3] : []);
      ctx.beginPath();
      ctx.moveTo(ox, oy);
      ctx.lineTo(rx, ry);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.fillStyle = color;
      ctx.beginPath();
      ctx.arc(ox, oy, 4, 0, Math.PI * 2);
      ctx.fill();
      ctx.beginPath();
      ctx.arc(rx, ry, 4, 0, Math.PI * 2);
      ctx.fill();
    }

    if (target) {
      const [sx, sy] = worldToCanvas(target.x, target.y, geom);
      drawCross(sx, sy, m.target);
    }
  }

  let drawPending = false;
  function requestDraw() {
    if (drawPending) {
      return;
    }
    drawPending = true;
    requestAnimationFrame(() => {
      drawPending = false;
      draw();
    });
  }

  function zoomAt(clientX, clientY, factor) {
    const rect = canvas.getBoundingClientRect();
    const mx = clientX - rect.left;
    const my = clientY - rect.top;
    const oldScale = fitScale() * userScale;
    const cx = rect.width / 2 + offset.x;
    const cy = rect.height / 2 + offset.y;
    const imgX = (mx - cx) / oldScale;
    const imgY = (my - cy) / oldScale;
    userScale = Math.min(8, Math.max(0.5, userScale * factor));
    const newScale = fitScale() * userScale;
    offset.x = mx - imgX * newScale - rect.width / 2;
    offset.y = my - imgY * newScale - rect.height / 2;
    draw();
  }

  canvas.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      zoomAt(e.clientX, e.clientY, e.deltaY < 0 ? 1.1 : 1 / 1.1);
    },
    { passive: false },
  );

  canvas.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    if (!onRightClickHandler) {
      return;
    }
    const [wx, wy] = screenToWorld(e.clientX, e.clientY);
    onRightClickHandler(wx, wy);
  });

  // Pointer tracking doubles as drag-to-pan (one pointer) and pinch-to-zoom (two pointers); a
  // left-button press that moves less than `CLICK_SLOP` before release counts as a click instead
  // of a pan (`touch-action: none` in app.css hands both gestures to us instead of the browser).
  const CLICK_SLOP = 6;
  const pointers = new Map();
  let dragging = false;
  let last = null;
  let pinchStartDist = null;
  let pinchStartScale = null;
  let downAt = null;
  let moved = 0;

  canvas.addEventListener("pointerdown", (e) => {
    canvas.setPointerCapture(e.pointerId);
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pointers.size === 1) {
      dragging = true;
      last = { x: e.clientX, y: e.clientY };
      if (e.button === 0) {
        downAt = { x: e.clientX, y: e.clientY };
        moved = 0;
      }
    } else if (pointers.size === 2) {
      dragging = false;
      downAt = null;
      const [a, b] = [...pointers.values()];
      pinchStartDist = Math.hypot(a.x - b.x, a.y - b.y);
      pinchStartScale = userScale;
    }
  });
  canvas.addEventListener("pointermove", (e) => {
    if (!pointers.has(e.pointerId)) {
      return;
    }
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      const dist = Math.hypot(a.x - b.x, a.y - b.y);
      if (pinchStartDist) {
        userScale = Math.min(8, Math.max(0.5, pinchStartScale * (dist / pinchStartDist)));
        draw();
      }
      return;
    }
    if (!dragging) {
      return;
    }
    const dx = e.clientX - last.x;
    const dy = e.clientY - last.y;
    offset.x += dx;
    offset.y += dy;
    last = { x: e.clientX, y: e.clientY };
    if (downAt) {
      moved += Math.abs(dx) + Math.abs(dy);
    }
    draw();
  });
  const releasePointer = (e) => {
    if (downAt && moved < CLICK_SLOP && e.button === 0 && onClickHandler) {
      const [wx, wy] = screenToWorld(e.clientX, e.clientY);
      onClickHandler(wx, wy);
    }
    downAt = null;
    pointers.delete(e.pointerId);
    if (pointers.size < 2) {
      pinchStartDist = null;
    }
    if (pointers.size === 1) {
      const [p] = pointers.values();
      dragging = true;
      last = { x: p.x, y: p.y };
    } else {
      dragging = false;
    }
  };
  canvas.addEventListener("pointerup", releasePointer);
  canvas.addEventListener("pointercancel", (e) => {
    downAt = null;
    releasePointer(e);
  });

  canvas.tabIndex = 0;
  canvas.addEventListener("keydown", (e) => {
    const step = 40;
    switch (e.key) {
      case "ArrowLeft":
        offset.x += step;
        break;
      case "ArrowRight":
        offset.x -= step;
        break;
      case "ArrowUp":
        offset.y += step;
        break;
      case "ArrowDown":
        offset.y -= step;
        break;
      case "+":
      case "=":
        userScale = Math.min(8, userScale * 1.1);
        break;
      case "-":
        userScale = Math.max(0.5, userScale / 1.1);
        break;
      default:
        return;
    }
    e.preventDefault();
    draw();
  });

  const resizeObserver = new ResizeObserver(draw);
  resizeObserver.observe(canvas.parentElement);

  // `resize`/ResizeObserver only fire on a CSS-box change; moving the window to a monitor with a
  // different scale factor leaves the same box but a different `devicePixelRatio`, which needs
  // its own watch (the standard `matchMedia(resolution)` idiom - it fires once, so re-arm it).
  // `dprMql`/`dprHandler` track whichever listener is currently pending, so `destroy()` can drop
  // it instead of leaking it forever (AMBER-7).
  let dprMql = null;
  let dprHandler = null;
  function watchDpr() {
    dprMql = matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
    dprHandler = () => {
      draw();
      watchDpr();
    };
    dprMql.addEventListener("change", dprHandler, { once: true });
  }
  watchDpr();

  draw();

  return {
    recolor(newTheme) {
      theme = newTheme;
      source = recolorImage(img, theme);
      draw();
    },
    setTarget(t) {
      target = t;
      requestDraw();
    },
    clearTarget() {
      target = null;
      requestDraw();
    },
    setOrigin(o) {
      origin = o;
      requestDraw();
    },
    clearOrigin() {
      origin = null;
      requestDraw();
    },
    setLineups(list) {
      lineups = list;
      requestDraw();
    },
    setSelected(id) {
      selectedId = id;
      requestDraw();
    },
    setHover(id) {
      hoverId = id;
      requestDraw();
    },
    addCheckedPoints(pts) {
      for (const p of pts) {
        checkedPoints.push(p);
      }
      requestDraw();
    },
    addVerifiedPoints(pts) {
      for (const p of pts) {
        (p.ok ? verifiedPoints : verifiedFailPoints).push(p);
      }
      requestDraw();
    },
    clearPoints() {
      checkedPoints = [];
      verifiedPoints = [];
      verifiedFailPoints = [];
      requestDraw();
    },
    onClick(cb) {
      onClickHandler = cb;
    },
    onRightClick(cb) {
      onRightClickHandler = cb;
    },
    destroy() {
      resizeObserver.disconnect();
      if (dprMql && dprHandler) {
        dprMql.removeEventListener("change", dprHandler);
      }
    },
  };
}
