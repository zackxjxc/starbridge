'use strict';

// Native desktop copy uses the main window's resolved language, independent of WebView storage.
window.renderTrayNotice = copy => {
  document.documentElement.lang = copy.locale;
  document.getElementById('notice-title').textContent = copy.title;
  document.getElementById('notice-body').textContent = copy.body;
  document.body.hidden = false;
};
if (window.__STARBRIDGE_TRAY_NOTICE__) window.renderTrayNotice(window.__STARBRIDGE_TRAY_NOTICE__);
else document.body.hidden = false;
