// The dashboard follows gatefold run's events: progress redraws the
// "Now" panel; anything else also reloads the status block. After a
// reconnect, both catch up.
(function () {
  const connection = document.getElementById("connection");
  let timer = null;
  let statusToo = false;
  function connected(yes) {
    if (!connection) return;
    connection.classList.toggle("stale", !yes);
    connection.textContent = yes ? "Live" : "Reconnecting";
  }
  function refresh(all) {
    statusToo = statusToo || all;
    if (timer) return;
    timer = setTimeout(function () {
      const status = statusToo;
      timer = null;
      statusToo = false;
      htmx.ajax("GET", "/fragments/live", { target: "#live", swap: "outerHTML" });
      if (status) htmx.ajax("GET", "/fragments/status", { target: "#status", swap: "outerHTML" });
    }, 300);
  }
  function connect() {
    const events = new EventSource("/api/v1/events");
    events.onopen = function () { connected(true); refresh(true); };
    events.onerror = function () { connected(false); };
    events.onmessage = function (message) {
      let event = {};
      try { event = JSON.parse(message.data); } catch (_) {}
      refresh(!(event.type === "progress" || event.type === "waiting" || event.type === "line"));
    };
  }
  if (window.EventSource) connect();
  else connected(false);
})();
