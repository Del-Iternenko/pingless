// Makes the webview behave like a desktop app instead of a browser page:
// no browser context menu, no reload/print/find/devtools/zoom shortcuts, no dragging
// links or images out, no text selection outside input fields.
"use strict";

(() => {
  const editable = (el) => el?.closest?.("input, textarea, [contenteditable='true'], .xterm");

  // Right click: only text fields keep the native Cut/Copy/Paste menu.
  document.addEventListener("contextmenu", (e) => {
    if (!editable(e.target)) e.preventDefault();
  });

  const blocked = (e) => {
    // Inside the terminal every key belongs to the shell (Ctrl+R, Ctrl+L, F5 in mc...);
    // xterm.js cancels the browser action for keys it handles.
    if (e.target.closest?.(".xterm")) return false;
    const k = e.key.toLowerCase();
    const ctrl = e.ctrlKey || e.metaKey;
    if (["f3", "f5", "f7", "f12", "browserback", "browserforward", "browserrefresh", "browserhome"].includes(k)) return true;
    if (e.altKey && (k === "arrowleft" || k === "arrowright" || k === "home")) return true;
    if (ctrl && e.shiftKey && ["i", "j", "c", "r", "delete", "n", "o", "b"].includes(k)) return true;
    if (ctrl && ["r", "p", "f", "g", "u", "j", "h", "s", "o", "n", "t", "w", "d", "e", "l", "+", "-", "=", "0"].includes(k)) {
      // Keep clipboard and select-all working inside text fields.
      return !(editable(e.target) && ["a", "c", "v", "x", "z", "y"].includes(k));
    }
    return false;
  };
  document.addEventListener("keydown", (e) => {
    if (blocked(e)) {
      e.preventDefault();
      e.stopPropagation();
    }
  }, true);

  // Ctrl+wheel and pinch would zoom the page.
  document.addEventListener("wheel", (e) => { if (e.ctrlKey) e.preventDefault(); }, { passive: false });

  // Dropping a file would navigate away from the app; dragging out links/images too.
  for (const ev of ["dragover", "drop"]) document.addEventListener(ev, (e) => e.preventDefault());
  document.addEventListener("dragstart", (e) => { if (!editable(e.target)) e.preventDefault(); });

  // Middle-click autoscroll and opening links in "new windows".
  document.addEventListener("auxclick", (e) => { if (e.button === 1) e.preventDefault(); });
  document.addEventListener("mousedown", (e) => { if (e.button === 1) e.preventDefault(); });
  document.addEventListener("click", (e) => {
    const a = e.target.closest?.("a[href]");
    if (a && !a.getAttribute("href").startsWith("#")) e.preventDefault();
  });
})();
