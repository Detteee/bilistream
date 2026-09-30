// theme.js — runs before first paint so a reload never flashes the wrong
// background. Not a module and not inline: both listeners's CSP allows
// script-src 'self' only.
(function () {
  try {
    if (localStorage.getItem('theme') === 'light') {
      document.documentElement.classList.add('light-theme');
    }
  } catch (e) {
    /* storage unavailable */
  }
})();
