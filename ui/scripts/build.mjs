import { copyFileSync } from "node:fs";

// The Rust server embeds ui/dist and serves /management.html.
// Vite empties dist/, so restore that entry point after each build.
copyFileSync("dist/index.html", "dist/management.html");
