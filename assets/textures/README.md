# Portal textures

`portal-source.gif` is the user-provided original: 30 frames of 128×128 pixels,
each displayed for 50 ms.

`nether-portal-frames.png` contains all frames stacked vertically. Each frame is
reduced to 8×8 with nearest-neighbor sampling; the complete atlas is 8×240.
`PortalSurface` advances one shared frame every 50 ms and `PortalTile` clips the
corresponding 8×8 region. Keep frame count, tile size and timing consistent when
replacing the animation. The original static `nether-portal.png` remains as a
16×16 reference frame.

Obsidian textures retain the provided 16×16 source resolution and are displayed
as 8×8 blocks with pixelated image rendering.
