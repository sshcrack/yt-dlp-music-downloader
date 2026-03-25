/**
 * YouTube Music Downloader — content script
 *
 * Injects a "▼ MP3 herunterladen" button next to the video title on every
 * YouTube /watch page.  Clicking the button opens the custom URI
 *
 *   ytdlpmusic://<video_id>
 *
 * which Windows hands off to the yt-music-downloader desktop application that
 * downloads the video as an MP3 file.
 *
 * YouTube is a Single-Page Application: the page does not reload when the user
 * navigates to a new video.  A MutationObserver watches for DOM changes and
 * re-injects (or updates) the button whenever the video ID changes.
 */

(function () {
  "use strict";

  /** Extract the "v" query-parameter from the current URL, or null. */
  function currentVideoId() {
    return new URLSearchParams(window.location.search).get("v");
  }

  /** Remove any button that was injected for a previous video. */
  function removeButton() {
    const old = document.getElementById("__ytdl_btn");
    if (old) old.remove();
  }

  /** Create and insert the download button for the given video ID. */
  function injectButton(videoId) {
    if (document.getElementById("__ytdl_btn")) return; // already present

    const titleEl = document.querySelector(
      "#above-the-fold > div:nth-child(1)"
    );
    if (!titleEl) return; // title area not rendered yet

    const btn = document.createElement("button");
    btn.id = "__ytdl_btn";
    btn.textContent = "\u25BC\u00A0MP3 herunterladen";

    Object.assign(btn.style, {
      background: "linear-gradient(135deg, #b80000, #ff2222)",
      color: "#ffffff",
      border: "none",
      padding: "7px 18px",
      fontSize: "13px",
      fontWeight: "700",
      fontFamily: '"YouTube Sans", Roboto, Arial, sans-serif',
      borderRadius: "18px",
      cursor: "pointer",
      marginLeft: "14px",
      verticalAlign: "middle",
      display: "inline-block",
      boxShadow: "0 2px 8px rgba(0,0,0,.35)",
      transition: "opacity .15s ease",
      whiteSpace: "nowrap",
      letterSpacing: ".3px",
    });

    btn.addEventListener("mouseover", () => (btn.style.opacity = "0.8"));
    btn.addEventListener("mouseout", () => (btn.style.opacity = "1"));

    btn.addEventListener("click", () => {
      // Give immediate visual feedback before the OS dialog appears.
      btn.textContent = "\u2713\u00A0Starte\u2026";
      btn.disabled = true;
      btn.style.background = "#555";
      btn.style.cursor = "default";
      btn.style.opacity = "1";

      // Open the custom URI — Windows will launch yt-music-downloader.exe.
      window.location.href = "ytdlpmusic://" + videoId;
    });

    titleEl.appendChild(btn);
  }

  /** Try to inject the button; called after every DOM mutation. */
  let lastVideoId = "";

  function tryInject() {
    const videoId = currentVideoId();
    if (!videoId) return;

    if (videoId !== lastVideoId) {
      // New video — remove the old button so a fresh one is created.
      removeButton();
      lastVideoId = videoId;
    }

    injectButton(videoId);
  }

  // Watch for DOM changes caused by YouTube's SPA navigation.
  const observer = new MutationObserver(tryInject);
  observer.observe(document.documentElement, {
    subtree: true,
    childList: true,
  });

  // Also run immediately in case the page was already loaded.
  tryInject();
})();
