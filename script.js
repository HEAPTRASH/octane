(() => {
  "use strict";

  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const root = document.documentElement;

  // Short mechanical boot transition. The page remains usable if JavaScript is disabled.
  const boot = document.querySelector(".boot-screen");
  const bootCount = document.querySelector(".boot-count");
  if (boot && !reducedMotion) {
    let frame = 0;
    const bootTimer = window.setInterval(() => {
      frame = Math.min(frame + Math.ceil((100 - frame) * 0.24), 100);
      if (bootCount) bootCount.textContent = String(frame).padStart(2, "0");
      if (frame >= 100) {
        window.clearInterval(bootTimer);
        boot.classList.add("is-done");
      }
    }, 32);
  } else {
    boot?.classList.add("is-done");
  }

  // Reveal content when it reaches the working area.
  const revealItems = [...document.querySelectorAll(".reveal")];
  if ("IntersectionObserver" in window && !reducedMotion) {
    const revealObserver = new IntersectionObserver(
      (entries, observer) => {
        entries.forEach((entry) => {
          if (!entry.isIntersecting) return;
          entry.target.classList.add("is-visible");
          observer.unobserve(entry.target);
        });
      },
      { threshold: 0.12, rootMargin: "0px 0px -6% 0px" }
    );
    revealItems.forEach((item) => revealObserver.observe(item));
  } else {
    revealItems.forEach((item) => item.classList.add("is-visible"));
  }

  // Oversized words drift sideways with scroll, preserving the industrial marquee feel.
  const driftItems = [...document.querySelectorAll("[data-drift]")];
  let driftFrame = null;
  const updateDrift = () => {
    const scrollY = window.scrollY;
    driftItems.forEach((item) => {
      const speed = Number.parseFloat(item.dataset.drift || "0");
      item.style.transform = `translate3d(${scrollY * speed}px, 0, 0)`;
    });
    driftFrame = null;
  };

  if (!reducedMotion && driftItems.length) {
    window.addEventListener(
      "scroll",
      () => {
        if (driftFrame === null) driftFrame = window.requestAnimationFrame(updateDrift);
      },
      { passive: true }
    );
    updateDrift();
  }

  // The app capture reacts as a physical panel, not as a fake UI.
  const tiltPanel = document.querySelector("[data-tilt]");
  if (tiltPanel && !reducedMotion && window.matchMedia("(pointer: fine)").matches) {
    tiltPanel.addEventListener("pointermove", (event) => {
      const rect = tiltPanel.getBoundingClientRect();
      const x = (event.clientX - rect.left) / rect.width - 0.5;
      const y = (event.clientY - rect.top) / rect.height - 0.5;
      tiltPanel.style.transform = `perspective(1400px) rotateX(${-y * 2.4}deg) rotateY(${x * 2.4}deg)`;
    });
    tiltPanel.addEventListener("pointerleave", () => {
      tiltPanel.style.transform = "";
    });
  }

  // Custom crosshair only appears with precise pointing devices.
  const crosshair = document.querySelector(".crosshair");
  if (crosshair && !reducedMotion && window.matchMedia("(pointer: fine)").matches) {
    window.addEventListener("pointermove", (event) => {
      crosshair.style.left = `${event.clientX}px`;
      crosshair.style.top = `${event.clientY}px`;
      crosshair.classList.add("is-visible");
    });
    document.querySelectorAll("a, button, [data-tilt]").forEach((target) => {
      target.addEventListener("pointerenter", () => crosshair.classList.add("is-active"));
      target.addEventListener("pointerleave", () => crosshair.classList.remove("is-active"));
    });
  }

  // Mobile navigation.
  const menuButton = document.querySelector(".menu-button");
  const siteNav = document.querySelector(".site-nav");
  const closeMenu = () => {
    menuButton?.setAttribute("aria-expanded", "false");
    siteNav?.classList.remove("is-open");
    document.body.classList.remove("menu-open");
  };

  menuButton?.addEventListener("click", () => {
    const nextOpen = menuButton.getAttribute("aria-expanded") !== "true";
    menuButton.setAttribute("aria-expanded", String(nextOpen));
    siteNav?.classList.toggle("is-open", nextOpen);
    document.body.classList.toggle("menu-open", nextOpen);
  });
  siteNav?.querySelectorAll("a").forEach((link) => link.addEventListener("click", closeMenu));
  window.addEventListener("resize", () => {
    if (window.innerWidth > 1050) closeMenu();
  });

  // Copy the real local build sequence.
  document.querySelectorAll("[data-copy]").forEach((button) => {
    button.addEventListener("click", async () => {
      const value = button.dataset.copy || "";
      try {
        await navigator.clipboard.writeText(value);
        const original = button.textContent;
        button.textContent = "COPIED";
        window.setTimeout(() => {
          button.textContent = original;
        }, 1600);
      } catch {
        button.textContent = "SELECT CODE";
      }
    });
  });

  // Keep keyboard focus visible without forcing a ring on pointer interactions.
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") closeMenu();
  });

  root.classList.add("js-ready");
})();
