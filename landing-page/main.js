const menuToggle = document.querySelector(".menu-toggle");
const mobileMenu = document.querySelector(".mobile-menu");
const menuBackdrop = document.querySelector(".mobile-menu-backdrop");
const navLinks = document.querySelectorAll("[data-nav-target]");
const mobileMenuLinks = document.querySelectorAll(".mobile-nav-link, .mobile-sign-in");
const statValues = document.querySelectorAll(".stat-value");
const contentSections = document.querySelectorAll(
  "#platform, #governance, #learning, #benchmarks, #security",
);
const scrollRevealItems = document.querySelectorAll(".reveal-on-scroll");
const prefersReducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

function openMenu() {
  mobileMenu.hidden = false;
  menuToggle.setAttribute("aria-expanded", "true");
  menuToggle.setAttribute("aria-label", "Close navigation");
  document.body.classList.add("menu-open");
}

function closeMenu({ returnFocus = false } = {}) {
  if (mobileMenu.hidden) return;

  mobileMenu.hidden = true;
  menuToggle.setAttribute("aria-expanded", "false");
  menuToggle.setAttribute("aria-label", "Open navigation");
  document.body.classList.remove("menu-open");

  if (returnFocus) menuToggle.focus();
}

menuToggle.addEventListener("click", () => {
  if (mobileMenu.hidden) openMenu();
  else closeMenu();
});

menuBackdrop.addEventListener("click", () => closeMenu({ returnFocus: true }));

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !mobileMenu.hidden) {
    closeMenu({ returnFocus: true });
  }
});

window.addEventListener("resize", () => {
  if (window.innerWidth > 720) closeMenu();
});

function setActiveNav(target) {
  navLinks.forEach((link) => {
    const isActive = link.dataset.navTarget === target;
    link.classList.toggle("active", isActive);
    if (isActive) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  });
}

navLinks.forEach((link) => {
  link.addEventListener("click", () => {
    setActiveNav(link.dataset.navTarget);
    closeMenu();
  });
});

if ("IntersectionObserver" in window) {
  const sectionObserver = new IntersectionObserver(
    (entries) => {
      const visibleEntry = entries.find((entry) => entry.isIntersecting);
      if (visibleEntry) setActiveNav(visibleEntry.target.id);
    },
    { rootMargin: "-24% 0px -66%", threshold: 0 },
  );

  contentSections.forEach((section) => sectionObserver.observe(section));
} else {
  window.addEventListener(
    "scroll",
    () => {
      let current = "platform";
      contentSections.forEach((section) => {
        if (section.getBoundingClientRect().top <= window.innerHeight * 0.4) current = section.id;
      });
      setActiveNav(current);
    },
    { passive: true },
  );
}

if (prefersReducedMotion.matches || !("IntersectionObserver" in window)) {
  scrollRevealItems.forEach((item) => item.classList.add("is-visible"));
} else {
  const revealObserver = new IntersectionObserver(
    (entries, observer) => {
      entries.forEach((entry) => {
        if (!entry.isIntersecting) return;
        entry.target.classList.add("is-visible");
        observer.unobserve(entry.target);
      });
    },
    { rootMargin: "0px 0px -10%", threshold: 0.12 },
  );

  scrollRevealItems.forEach((item) => revealObserver.observe(item));
}

mobileMenuLinks.forEach((link) => {
  link.addEventListener("click", () => closeMenu());
});

function setFinalValue(element) {
  const target = Number(element.dataset.count);
  const suffix = element.dataset.suffix || "";
  const decimals = Number(element.dataset.decimals || 0);
  element.textContent = `${target.toFixed(decimals)}${suffix}`;
}

function animateValue(element, index) {
  const target = Number(element.dataset.count);
  const suffix = element.dataset.suffix || "";
  const decimals = Number(element.dataset.decimals || 0);
  const duration = 1500 + index * 80;
  const startDelay = 480 + index * 90;

  window.setTimeout(() => {
    const startedAt = performance.now();

    function update(now) {
      const progress = Math.min((now - startedAt) / duration, 1);
      const eased = 1 - Math.pow(1 - progress, 3);
      const current = target * eased;
      element.textContent = `${current.toFixed(decimals)}${suffix}`;

      if (progress < 1) requestAnimationFrame(update);
      else setFinalValue(element);
    }

    requestAnimationFrame(update);
  }, startDelay);
}

if (prefersReducedMotion.matches) {
  statValues.forEach(setFinalValue);
} else {
  const statsObserver = new IntersectionObserver(
    (entries, observer) => {
      entries.forEach((entry) => {
        if (!entry.isIntersecting) return;

        statValues.forEach((element, index) => animateValue(element, index));
        observer.disconnect();
      });
    },
    { threshold: 0.25 },
  );

  statsObserver.observe(document.querySelector(".stats"));
}
