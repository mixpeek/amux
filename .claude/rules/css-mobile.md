---
description: When editing CSS in amux-server.py — mobile and PWA considerations
globs: ["amux-server.py"]
---

When editing CSS:
- Check that any new UI fits within the existing `@media (max-width: 600px)` breakpoints
- Touch targets must be at least 44x44 on mobile
- Use `env(safe-area-inset-*)` for anything positioned at screen edges (iOS PWA notch)
- The viewport uses `viewport-fit=cover` — fixed overlays must extend to physical edges
- Test that flex containers don't overflow on 375px-wide screens
