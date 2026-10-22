let activeHelp = null,
  helpPinned = false;
function hideHelp() {
  motionExit(document.querySelector('.help-popover'));
  activeHelp?.setAttribute('aria-expanded', 'false');
  activeHelp = null;
  helpPinned = false;
}
function positionHelp() {
  const box = document.querySelector('.help-popover');
  if (!box || !activeHelp?.isConnected) {
    hideHelp();
    return;
  }
  const r = activeHelp.getBoundingClientRect();
  if (r.bottom < 0 || r.top > innerHeight) {
    hideHelp();
    return;
  }
  const b = box.getBoundingClientRect();
  box.style.left = Math.max(12, Math.min(r.left, innerWidth - b.width - 12)) + 'px';
  box.style.top =
    (r.bottom + b.height + 12 < innerHeight ? r.bottom + 8 : Math.max(12, r.top - b.height - 8)) +
    'px';
}
function showHelp(el, pin = false) {
  if (activeHelp === el) {
    helpPinned = helpPinned || pin;
    positionHelp();
    return;
  }
  hideHelp();
  activeHelp = el;
  helpPinned = pin;
  const box = document.createElement('div');
  box.className = 'help-popover';
  box.id = 'help-popover';
  box.role = 'tooltip';
  box.textContent = el.dataset.help || el.dataset.tip || '';
  document.body.append(box);
  el.setAttribute('aria-expanded', 'true');
  el.setAttribute('aria-describedby', box.id);
  positionHelp();
}
document.addEventListener('pointerover', (e) => {
  const el = e.target.closest('.help-dot');
  if (el && !helpPinned) showHelp(el);
});
document.addEventListener('pointerout', (e) => {
  if (
    activeHelp &&
    !helpPinned &&
    document.activeElement !== activeHelp &&
    e.target.closest('.help-dot')
  )
    hideHelp();
});
document.addEventListener('focusin', (e) => {
  const el = e.target.closest('.help-dot');
  if (el) showHelp(el);
});
document.addEventListener('focusout', (e) => {
  if (e.target.closest('.help-dot') && !helpPinned) hideHelp();
});
document.addEventListener('click', (e) => {
  const el = e.target.closest('.help-dot');
  if (el) {
    e.preventDefault();
    if (activeHelp === el && helpPinned) hideHelp();
    else showHelp(el, true);
  } else hideHelp();
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') hideHelp();
});
window.addEventListener('resize', positionHelp);
document.addEventListener(
  'scroll',
  () => {
    if (helpPinned || document.activeElement === activeHelp) positionHelp();
    else hideHelp();
  },
  true,
);
// Sender avatar: first character on a colour picked stably from the name, so the
// same person is recognisable at a glance across pages and sessions.
function senderAvatar(name) {
  const text = String(name || '').trim(),
    tones = ['blue', 'green', 'amber', 'red', 'violet'];
  let h = 0;
  for (const c of text) h = (h * 31 + c.codePointAt(0)) >>> 0;
  return `<span class="sender-avatar tone-${tones[h % tones.length]}" aria-hidden="true">${esc([...text][0] || '?')}</span>`;
}
