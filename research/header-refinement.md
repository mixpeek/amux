# Header refinement — AF-750 and AF-751

The compact mobile header keeps the owner's `a`, live dot, red limited count and worker count. Notifications and settings use consistent line icons. The unread badge stays inside its own tap target. Secondary controls no longer look like a row of oversized bordered cards; Create remains the primary filled action. Desktop aligns the action group at the right while retaining the full limited/reset label.

On mobile the first four navigation labels fit completely beside the tab picker. The rest remain horizontally scrollable. Header buttons and the tab picker retain at least 44px tap targets. Existing handlers and accessible status labels remain intact.

`e2e/mobile-header-visibility.spec.ts`: six project/test combinations passed in desktop Chromium, mobile Chromium and Safari. Checks cover 320/375/402px native-style controls, both themes at 375/600/601/768/1440px, notification badge containment, primary navigation, real settings/add menu clicks, and a deliberately overflowing desktop badge that must emit a measured `mobile-header-clipped` diagnostic with `surface: desktop`.

Screenshots inspected at 375px and 1440px in light and dark themes: local `scratch/ios-simulator-review/header-final-*.png`. Native simulator evidence and the deployed commit/build are recorded on both cards. A server adoption interrupted the first native attempt; its partial evidence is not represented as a complete pass.
