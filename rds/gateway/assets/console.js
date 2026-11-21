// Console page controller. External file: the CSP only allows script-src 'self'.
//
// Loads the wasm-bindgen glue for uflinux-rds-web, reads the sign-in that
// login.js left in sessionStorage, builds the Rust client — which opens the
// WebSocket and renders into #screen itself — and forwards DOM events to it.
// All RDP logic, including scancode translation, lives in Rust; this file only
// reads DOM fields and scales coordinates into canvas space.
(function () {
  "use strict";

  var STORAGE_KEY = "uflinux-rds";
  var WASM_JS = "/assets/uflinux_rds_web.js";
  var WASM_BIN = "/assets/uflinux_rds_web_bg.wasm";
  var RESIZE_DEBOUNCE_MS = 300;
  var MAX_CANVAS_PX = 4096;

  var canvas = document.getElementById("screen");
  var screenWrap = document.getElementById("screen-wrap");
  var pill = document.getElementById("state");
  var status = document.getElementById("status");
  var overlay = document.getElementById("overlay");
  var overlayReason = document.getElementById("overlay-reason");
  var reconnectBtn = document.getElementById("reconnect");
  var toast = document.getElementById("toast");
  var fullscreenBtn = document.getElementById("fullscreen");
  var breakBtn = document.getElementById("send-break");
  var disconnectBtn = document.getElementById("disconnect");
  var pasteBtn = document.getElementById("paste");

  if (!canvas || !pill) {
    return;
  }

  var client = null; // wasm Client, set once connected
  var disposed = false;
  var remoteText = ""; // last clipboard text received from the server
  var localText = ""; // last clipboard text we sent to the server

  // --- UI helpers --------------------------------------------------------

  function setState(state, text) {
    pill.dataset.state = state;
    pill.textContent = text;
  }

  function say(message) {
    if (status) {
      status.textContent = message || "";
    }
  }

  var toastTimer = 0;
  function notify(message) {
    if (!toast) {
      return;
    }
    toast.textContent = message;
    toast.classList.add("show");
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(function () {
      toast.classList.remove("show");
    }, 1800);
  }

  function showOverlay(reason) {
    if (overlayReason) {
      overlayReason.textContent = reason;
    }
    if (overlay) {
      overlay.hidden = false;
    }
  }

  // --- canvas geometry ---------------------------------------------------

  // Keeps the canvas backing store in step with its CSS box. Returns true when
  // the store actually changed, i.e. when the server should hear about it.
  function fitCanvas() {
    var rect = canvas.getBoundingClientRect();
    var ratio = window.devicePixelRatio || 1;
    var width = Math.round(Math.max(1, rect.width) * ratio);
    var height = Math.round(Math.max(1, rect.height) * ratio);
    width = Math.min(width, MAX_CANVAS_PX);
    height = Math.min(height, MAX_CANVAS_PX);
    if (canvas.width === width && canvas.height === height) {
      return false;
    }
    canvas.width = width;
    canvas.height = height;
    return true;
  }

  // Maps a pointer event into canvas pixels, which are desktop coordinates.
  function desktopPoint(event) {
    var rect = canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) {
      return { x: 0, y: 0 };
    }
    return {
      x: ((event.clientX - rect.left) * canvas.width) / rect.width,
      y: ((event.clientY - rect.top) * canvas.height) / rect.height,
    };
  }

  function modifier(event, name) {
    return Boolean(event.getModifierState && event.getModifierState(name));
  }

  // --- input wiring ------------------------------------------------------

  // Input calls can throw: the façade returns a Result for every event, and
  // wasm-bindgen turns that into an exception. A dropped input must never
  // break the page, so each call site goes through here.
  function attempt(call) {
    try {
      return call();
    } catch (err) {
      return false;
    }
  }

  function sendKey(event, pressed) {
    if (!client) {
      return;
    }
    var handled = attempt(function () {
      return client.key(
        event.code,
        pressed,
        modifier(event, "ScrollLock"),
        modifier(event, "NumLock"),
        modifier(event, "CapsLock"),
        modifier(event, "KanaLock")
      );
    });
    // Only swallow the browser default for codes we actually translated.
    if (handled) {
      event.preventDefault();
    }
  }

  canvas.addEventListener("pointermove", function (event) {
    if (client) {
      var p = desktopPoint(event);
      attempt(function () {
        client.pointer_move(p.x, p.y, event.buttons);
      });
    }
  });

  canvas.addEventListener("pointerdown", function (event) {
    if (!client) {
      return;
    }
    event.preventDefault();
    canvas.focus();
    var p = desktopPoint(event);
    attempt(function () {
      client.button(true, event.button, p.x, p.y);
    });
  });

  function releaseButton(event) {
    if (client) {
      var p = desktopPoint(event);
      attempt(function () {
        client.button(false, event.button, p.x, p.y);
      });
    }
  }
  canvas.addEventListener("pointerup", releaseButton);
  canvas.addEventListener("pointercancel", releaseButton);

  canvas.addEventListener(
    "wheel",
    function (event) {
      if (!client) {
        return;
      }
      event.preventDefault();
      attempt(function () {
        client.wheel(event.deltaY, event.deltaX, event.deltaMode);
      });
    },
    { passive: false }
  );

  canvas.addEventListener("contextmenu", function (event) {
    event.preventDefault();
  });

  document.addEventListener("keydown", function (event) {
    var tag = event.target && event.target.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA" || tag === "BUTTON") {
      return;
    }
    sendKey(event, true);
  });

  document.addEventListener("keyup", function (event) {
    sendKey(event, false);
  });

  // Losing focus with buttons held would leave them stuck down remotely.
  window.addEventListener("blur", function () {
    if (client) {
      attempt(function () {
        client.release_all_inputs();
      });
    }
  });

  // Clipboard: the synchronous paste event needs no permission prompt, so it
  // is the primary local -> remote path. clipboard_paste() is the fallback.
  document.addEventListener("paste", function (event) {
    if (!client || !event.clipboardData) {
      return;
    }
    var text = event.clipboardData.getData("text/plain");
    if (text) {
      var sent = attempt(function () {
        client.clipboard_send_text(text);
        return true;
      });
      if (sent) {
        event.preventDefault();
        localText = text;
        notify("clipboard sent to session");
      }
    }
  });

  // Remote -> local: hand the last text the server sent us to the browser's
  // copy buffer when the user copies with the session focused.
  document.addEventListener("copy", function (event) {
    if (client && remoteText && event.clipboardData) {
      event.clipboardData.setData("text/plain", remoteText);
      event.preventDefault();
    }
  });

  // --- wasm callbacks ----------------------------------------------------

  // IronRDP calls this as (kind, data, hotspotX, hotspotY) where kind is
  // "default", "hidden" or "url" and data is a data: URL for the latter.
  function onPointerCursor(kind, data, hotspotX, hotspotY) {
    if (kind === "hidden") {
      canvas.style.cursor = "none";
    } else if (kind === "url" && typeof data === "string") {
      var hx = Number.isFinite(hotspotX) ? hotspotX : 0;
      var hy = Number.isFinite(hotspotY) ? hotspotY : 0;
      canvas.style.cursor = 'url("' + data + '") ' + hx + " " + hy + ", default";
    } else {
      canvas.style.cursor = "default";
    }
  }

  function onClipboardText(text) {
    remoteText = text;
    notify("remote clipboard ready — press Ctrl+C to keep it");
  }

  // --- session lifecycle -------------------------------------------------

  function disconnect(reason) {
    if (disposed) {
      return;
    }
    disposed = true;
    client = null;
    setState("error", "disconnected");
    showOverlay(reason || "Connection closed.");
  }

  async function start() {
    var stored = sessionStorage.getItem(STORAGE_KEY);
    if (!stored) {
      disconnect("No active sign-in. Open this page from the login screen.");
      return;
    }
    // Single-use token: consume it at once so a reload cannot reuse it.
    sessionStorage.removeItem(STORAGE_KEY);

    var signIn;
    try {
      signIn = JSON.parse(stored);
    } catch (err) {
      disconnect("Stored sign-in is unreadable. Sign in again.");
      return;
    }

    setState("connecting", "connecting");
    say("Loading RDP engine…");

    var mod = await import(WASM_JS);
    // Default export is wasm-bindgen's init; a bare string is fetched as the
    // module. Both files live under /assets/, so the default would also work.
    await mod.default(WASM_BIN);
    mod.init("warn");

    var options = new mod.ConnectOptions();
    options.wsUrl = signIn.ws_url;
    options.token = signIn.token;
    options.username = signIn.username || "";
    options.password = signIn.password || "";
    options.destination = signIn.destination || "";
    options.bridge = mod;
    options.canvas = canvas;
    options.onDisconnected = disconnect;
    options.onError = function (kind, message) {
      say(message);
    };
    options.onPointerCursor = onPointerCursor;
    options.onClipboardText = onClipboardText;
    options.onClipboardRequest = function () {
      if (client && localText) {
        attempt(function () {
          client.clipboard_send_text(localText);
        });
      }
    };

    fitCanvas(); // the initial desktop size is taken from the canvas
    say("Connecting…");

    var connected = await mod.connect(options);
    if (disposed) {
      connected.free();
      return;
    }
    client = connected;
    // start() returns undefined when the pump is running and throws when it
    // could not be started at all.
    if (attempt(function () {
      client.start();
      return true;
    }) !== true) {
      disconnect("The session ended before it started.");
      return;
    }
    setState("connected", "connected");
    say("");
    canvas.focus();
  }

  // --- toolbar -----------------------------------------------------------

  if (fullscreenBtn) {
    fullscreenBtn.addEventListener("click", function () {
      if (document.fullscreenElement) {
        document.exitFullscreen();
      } else if (screenWrap && screenWrap.requestFullscreen) {
        screenWrap.requestFullscreen();
      }
    });
  }

  if (breakBtn) {
    // Ctrl+Alt+Del as ordinary key events; the façade maps the codes.
    breakBtn.addEventListener("click", function () {
      if (!client) {
        return;
      }
      var sequence = [
        ["ControlLeft", true],
        ["AltLeft", true],
        ["Delete", true],
        ["Delete", false],
        ["AltLeft", false],
        ["ControlLeft", false],
      ];
      for (var i = 0; i < sequence.length; i += 1) {
        var step = sequence[i];
        attempt(function () {
          client.key(step[0], step[1], false, false, false, false);
        });
      }
      notify("ctrl-alt-del sent");
    });
  }

  if (pasteBtn) {
    pasteBtn.addEventListener("click", function () {
      if (client) {
        client.clipboard_paste();
        notify("clipboard requested");
      }
    });
  }

  if (disconnectBtn) {
    // stop() only asks for a shutdown; the reason arrives through
    // onDisconnected, which is what hides the session and shows the overlay.
    disconnectBtn.addEventListener("click", function () {
      if (client) {
        client.stop();
      }
    });
  }

  if (reconnectBtn) {
    reconnectBtn.addEventListener("click", function () {
      window.location.assign("/");
    });
  }

  // --- resize observation ------------------------------------------------

  var resizeTimer = 0;
  function scheduleResize() {
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(function () {
      if (fitCanvas() && client) {
        attempt(function () {
          client.resize(canvas.width, canvas.height);
        });
      }
    }, RESIZE_DEBOUNCE_MS);
  }

  if (screenWrap && typeof ResizeObserver === "function") {
    new ResizeObserver(scheduleResize).observe(screenWrap);
  }
  window.addEventListener("resize", scheduleResize);
  document.addEventListener("fullscreenchange", scheduleResize);

  // --- go ----------------------------------------------------------------

  start().catch(function (err) {
    disconnect(typeof err === "string" ? err : "Could not start the RDP session.");
  });
})();
