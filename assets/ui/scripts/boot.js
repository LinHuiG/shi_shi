// The sidebar follows the window width; a toggle holds until the width
// crosses the breakpoint again.
const navNarrow = matchMedia('(max-width: 1100px)');
let navChoice = null;
function syncNav() {
  const collapsed = navChoice ?? navNarrow.matches;
  $('.app').classList.toggle('nav-collapsed', collapsed);
  const toggle = $('#nav-toggle'),
    label = collapsed ? '展开导航' : '收起导航';
  toggle.setAttribute('aria-expanded', String(!collapsed));
  toggle.setAttribute('aria-label', label);
  toggle.title = label;
}
$('#nav-toggle').addEventListener('click', () => {
  navChoice = !$('.app').classList.contains('nav-collapsed');
  syncNav();
});
navNarrow.addEventListener('change', () => {
  navChoice = null;
  syncNav();
});
syncNav();
let notificationVersion = null,
  refreshTimer = null;
window.stateChanged = function (version) {
  if (JSON.stringify(notificationVersion) === JSON.stringify(version)) return;
  notificationVersion = version;
  if (refreshInFlight) {
    refreshQueued = true;
    return;
  }
  if (!refreshTimer)
    refreshTimer = setTimeout(() => {
      refreshTimer = null;
      void refreshState(false);
    }, 80);
};
let displayTimer = null;
function displayTick() {
  refreshTaskClock();
  if (state?.ai_busy) aiClock();
  if (state) {
    syncUpdateNotice();
    syncEditProgress();
  }
}
// Clocks only redraw what is on screen, so they stop while hidden.
function syncDisplayTimer() {
  if (document.hidden) {
    clearInterval(displayTimer);
    displayTimer = null;
  } else if (!displayTimer) {
    displayTimer = setInterval(displayTick, 1000);
    displayTick();
  }
}
document.addEventListener('visibilitychange', () => {
  syncDisplayTimer();
  if (!document.hidden) void refreshState(true);
});
refreshState(true);
syncDisplayTimer();
