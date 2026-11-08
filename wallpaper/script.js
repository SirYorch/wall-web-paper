(() => {
  const canvas = document.getElementById("particle-canvas");
  const ctx = canvas.getContext("2d");
  const clockEl = document.getElementById("clock");
  const dateEl = document.getElementById("date");
  const infoEl = document.getElementById("wayland-info");
  const themeBtn = document.getElementById("theme-btn");
  const burstBtn = document.getElementById("burst-btn");

  const palettes = [
    { start: "#0b0f19", mid: "#111827", end: "#1e1b4b", accent: "#38bdf8", secondary: "#a855f7" },
    { start: "#051914", mid: "#062c22", end: "#064e3b", accent: "#34d399", secondary: "#10b981" },
    { start: "#1a0b16", mid: "#2e1025", end: "#4c0519", accent: "#fb7185", secondary: "#f43f5e" },
    { start: "#0f172a", mid: "#1e293b", end: "#312e81", accent: "#fbbf24", secondary: "#f59e0b" }
  ];
  let paletteIndex = 0;

  function applyPalette(p) {
    const root = document.documentElement;
    root.style.setProperty("--bg-start", p.start);
    root.style.setProperty("--bg-mid", p.mid);
    root.style.setProperty("--bg-end", p.end);
    root.style.setProperty("--accent", p.accent);
    root.style.setProperty("--accent-secondary", p.secondary);
  }

  // Reloj en tiempo real
  function updateClock() {
    const now = new Date();
    clockEl.textContent = now.toLocaleTimeString("es-ES", {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit"
    });
    dateEl.textContent = now.toLocaleDateString("es-ES", {
      weekday: "long",
      day: "numeric",
      month: "long",
      year: "numeric"
    });
  }
  updateClock();
  setInterval(updateClock, 1000);

  // Mostrar metadatos inyectados por el motor Rust
  function updateMetadata(meta) {
    if (!meta) return;
    const mode = meta.interactuable ? "Interactivo" : "Solo vista";
    infoEl.textContent = `${meta.monitor} • ${meta.resolucion.ancho}×${meta.resolucion.alto} • ${mode}`;
  }

  if (window.__WALL_WEB_PAPER__) {
    updateMetadata(window.__WALL_WEB_PAPER__);
  }
  window.addEventListener("wall-web-paper:ready", (ev) => updateMetadata(ev.detail));

  // Sistema de partículas interactivas en Canvas
  let width = (canvas.width = window.innerWidth);
  let height = (canvas.height = window.innerHeight);

  window.addEventListener("resize", () => {
    width = canvas.width = window.innerWidth;
    height = canvas.height = window.innerHeight;
  });

  const mouse = { x: width / 2, y: height / 2, active: false };
  window.addEventListener("pointermove", (e) => {
    mouse.x = e.clientX;
    mouse.y = e.clientY;
    mouse.active = true;
  });
  window.addEventListener("pointerleave", () => {
    mouse.active = false;
  });

  const PARTICLE_COUNT = 85;
  const particles = Array.from({ length: PARTICLE_COUNT }, () => ({
    x: Math.random() * width,
    y: Math.random() * height,
    vx: (Math.random() - 0.5) * 1.1,
    vy: (Math.random() - 0.5) * 1.1,
    radius: Math.random() * 2.2 + 1.2
  }));

  function triggerBurst(cx, cy) {
    for (const p of particles) {
      const dx = p.x - cx;
      const dy = p.y - cy;
      const dist = Math.hypot(dx, dy) || 1;
      const force = Math.min(12, 260 / dist);
      p.vx += (dx / dist) * force;
      p.vy += (dy / dist) * force;
    }
  }

  window.addEventListener("pointerdown", (e) => {
    triggerBurst(e.clientX, e.clientY);
  });

  themeBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    paletteIndex = (paletteIndex + 1) % palettes.length;
    applyPalette(palettes[paletteIndex]);
  });

  burstBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    triggerBurst(width / 2, height / 2);
  });

  function render() {
    ctx.clearRect(0, 0, width, height);
    const accent = palettes[paletteIndex].accent;

    for (let i = 0; i < particles.length; i++) {
      const p = particles[i];
      p.x += p.vx;
      p.y += p.vy;

      // Amortiguación suave de velocidad
      p.vx *= 0.992;
      p.vy *= 0.992;
      if (Math.hypot(p.vx, p.vy) < 0.35) {
        p.vx += (Math.random() - 0.5) * 0.15;
        p.vy += (Math.random() - 0.5) * 0.15;
      }

      if (p.x < 0) p.x = width;
      if (p.x > width) p.x = 0;
      if (p.y < 0) p.y = height;
      if (p.y > height) p.y = 0;

      if (mouse.active) {
        const dx = mouse.x - p.x;
        const dy = mouse.y - p.y;
        const dist = Math.hypot(dx, dy);
        if (dist < 160 && dist > 1) {
          p.x += (dx / dist) * 0.65;
          p.y += (dy / dist) * 0.65;
        }
      }

      ctx.beginPath();
      ctx.arc(p.x, p.y, p.radius, 0, Math.PI * 2);
      ctx.fillStyle = accent;
      ctx.fill();

      for (let j = i + 1; j < particles.length; j++) {
        const q = particles[j];
        const dist = Math.hypot(p.x - q.x, p.y - q.y);
        if (dist < 130) {
          ctx.beginPath();
          ctx.moveTo(p.x, p.y);
          ctx.lineTo(q.x, q.y);
          ctx.strokeStyle = `${accent}${Math.floor((1 - dist / 130) * 90)
            .toString(16)
            .padStart(2, "0")}`;
          ctx.lineWidth = 0.8;
          ctx.stroke();
        }
      }
    }

    requestAnimationFrame(render);
  }

  render();
})();
