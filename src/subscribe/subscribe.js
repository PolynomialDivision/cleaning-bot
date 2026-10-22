// The feed page (src/subscribe.rs): a copy button for the address field. Without this
// script the field is still there to select and copy by hand.
document.addEventListener("DOMContentLoaded", function () {
  var field = document.getElementById("feed");
  var button = document.getElementById("copy");
  if (!field) return;
  field.addEventListener("focus", function () { field.select(); });
  if (!button || !navigator.clipboard) return;
  button.hidden = false;
  button.addEventListener("click", function () {
    navigator.clipboard.writeText(field.value).then(
      function () { button.textContent = button.getAttribute("data-done"); },
      function () { field.focus(); field.select(); }
    );
  });
});
