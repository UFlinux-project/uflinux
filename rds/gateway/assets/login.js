// Login form handler. External file: the CSP only allows script-src 'self'.
//
// POSTs the credentials to /api/login, keeps the returned one-time token in
// sessionStorage (never in the URL, so it cannot leak via referrer or history)
// and hands over to /session, where the Wasm client picks it up.
(function () {
  "use strict";

  var form = document.getElementById("login");
  var status = document.getElementById("status");
  var button = document.getElementById("submit");
  var userField = document.getElementById("username");
  var passField = document.getElementById("password");

  if (!form || !status || !button) {
    return;
  }

  function say(message, isError) {
    status.textContent = message;
    status.classList.toggle("error", Boolean(isError));
  }

  form.addEventListener("submit", function (event) {
    event.preventDefault();

    var username = userField.value.trim();
    if (!username || !passField.value) {
      say("Username and password are required.", true);
      return;
    }

    button.disabled = true;
    say("Signing in…", false);

    // Only the two fields go on the wire; nothing is logged.
    fetch("/api/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "same-origin",
      body: JSON.stringify({ username: username, password: passField.value }),
    })
      .then(function (response) {
        return response
          .json()
          .catch(function () {
            return {};
          })
          .then(function (data) {
            return { ok: response.ok, data: data };
          });
      })
      .then(function (result) {
        if (!result.ok) {
          button.disabled = false;
          passField.select();
          say(result.data.error || "Sign-in failed.", true);
          return;
        }
        try {
          // The password rides along because RDP's own logon still needs it
          // and the gateway token does not carry it. the Wasm client deletes the
          // whole record before it connects, so it lives in storage only for
          // the redirect.
          sessionStorage.setItem(
            "uflinux-rds",
            JSON.stringify({
              token: result.data.token,
              ws_url: result.data.ws_url,
              destination: result.data.destination,
              username: username,
              password: passField.value,
            })
          );
        } catch (err) {
          // Private-mode storage failures surface as a console error instead
          // of a redirect loop into a session page that cannot find its token.
          button.disabled = false;
          say("This browser blocks session storage; sign-in cannot continue.", true);
          return;
        }
        say("Connected. Opening session…", false);
        window.location.assign("/session");
      })
      .catch(function () {
        button.disabled = false;
        say("Network error. Check the connection and try again.", true);
      });
  });

  userField.focus();
})();
