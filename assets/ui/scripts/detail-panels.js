// Long record disclosures use the same bottom control and scroll boundary.
function detailCollapseFooter(label) {
  return `<div class="detail-panel-footer"><button class="quiet" data-detail-collapse aria-label="收起${label}">收起 ↑</button></div>`;
}
function measureDetailPanels() {
  for (const panel of document.querySelectorAll('details.detail-panel')) {
    const pane = panel.closest('.detail');
    panel.style.setProperty(
      '--detail-bottom-inset',
      pane ? getComputedStyle(pane).paddingBottom : '0px',
    );
  }
}
let detailPanelFrame = 0;
function scheduleDetailPanels() {
  if (!detailPanelFrame)
    detailPanelFrame = requestAnimationFrame(() => {
      detailPanelFrame = 0;
      measureDetailPanels();
    });
}
new MutationObserver(scheduleDetailPanels).observe($('#view'), { childList: true, subtree: true });
window.addEventListener('resize', scheduleDetailPanels);
document.addEventListener('click', (event) => {
  const button = event.target.closest('[data-detail-collapse]');
  if (!button) return;
  const panel = button.closest('details.detail-panel'),
    pane = panel.closest('.detail'),
    summary = panel.querySelector('summary');
  const viewport = pane.getBoundingClientRect(),
    before = summary.getBoundingClientRect(),
    top = viewport.top + parseFloat(getComputedStyle(pane).paddingTop);
  const anchor = Math.max(top, Math.min(viewport.bottom - before.height, before.top));
  summary.click();
  pane.scrollTop += summary.getBoundingClientRect().top - anchor;
  summary.focus({ preventScroll: true });
});
