// A reader owns its rows only while its page or window remains open.
function pageFeed({ root, read, key, render, alive = () => root.isConnected }) {
  const feed = {
    rows: [],
    data: null,
    page: 0,
    cursor: null,
    done: false,
    loading: false,
    error: '',
    epoch: 0,
    disposed: false,
  };
  let pending = null,
    workingRows = null,
    keys = new Set(),
    retryRefresh = false,
    refreshQueued = false,
    lastScroll = root.scrollTop;
  const current = (epoch) => !feed.disposed && feed.epoch === epoch && alive();
  const paint = () => {
    if (feed.disposed || !alive()) return;
    const edge = root.getBoundingClientRect().top;
    const anchor = [...root.querySelectorAll('[data-feed-key]')].find((el) => el.getBoundingClientRect().bottom > edge);
    const anchorKey = anchor?.dataset.feedKey,
      offset = anchor?.getBoundingClientRect().top;
    const opened = new Map(
      [...root.querySelectorAll('[data-feed-key]')].map((el) => [
        el.dataset.feedKey,
        [...el.querySelectorAll('details')].map((d) => d.open),
      ]),
    );
    render(feed);
    for (const el of root.querySelectorAll('[data-feed-key]')) {
      const values = opened.get(el.dataset.feedKey);
      if (values)
        [...el.querySelectorAll('details')].forEach((d, i) => {
          if (i < values.length) d.open = values[i];
        });
    }
    const next =
      anchorKey && [...root.querySelectorAll('[data-feed-key]')].find((el) => el.dataset.feedKey === anchorKey);
    if (next) root.scrollTop += next.getBoundingClientRect().top - offset;
    // Restoring an anchor or revealing a selected row must not fetch another batch.
    lastScroll = root.scrollTop;
  };
  feed.load = (refresh = false) => {
    if (feed.disposed || !alive()) return Promise.resolve();
    if (feed.loading) {
      if (!refresh) return pending || Promise.resolve();
      refreshQueued = true;
      return (pending || Promise.resolve()).then(() => {
        if (refreshQueued) {
          refreshQueued = false;
          return feed.load(true);
        }
      });
    }
    refresh = refresh || (feed.error && retryRefresh);
    if (!refresh && feed.done && !feed.error) return Promise.resolve();
    const epoch = refresh ? ++feed.epoch : feed.epoch,
      target = refresh ? Math.max(50, feed.rows.length) : 0;
    retryRefresh = !!refresh;
    feed.loading = true;
    feed.error = '';
    paint();
    pending = (async () => {
      let rows = refresh ? [] : feed.rows.slice(),
        known = refresh ? new Set() : new Set(keys);
      workingRows = rows;
      let nextPage = refresh ? 1 : feed.page + 1,
        cursor = refresh ? null : feed.cursor;
      try {
        do {
          const result = await read({ page: nextPage, page_size: 50, ...(cursor != null ? { cursor } : {}) });
          if (!current(epoch)) return;
          // Every reader pages by keyset: `next_cursor` is null on the last page.
          const { rows: batch, ...data } = result,
            returnedPage = data.page;
          let added = 0;
          for (const row of batch) {
            const id = String(key(row));
            if (!known.has(id)) {
              known.add(id);
              rows.push(row);
              added++;
            }
          }
          const done = !batch.length || !added || data.next_cursor == null;
          cursor = data.next_cursor;
          nextPage = returnedPage + 1;
          if (!refresh || done || rows.length >= target) {
            feed.rows = rows;
            keys = known;
            feed.data = data;
            feed.page = returnedPage;
            feed.cursor = cursor;
            feed.done = done;
            break;
          }
        } while (current(epoch));
      } catch (error) {
        if (current(epoch)) feed.error = error.message;
      } finally {
        workingRows = null;
        if (current(epoch)) {
          feed.loading = false;
          pending = null;
          paint();
        }
      }
    })();
    return pending;
  };
  const onScroll = () => {
    if (Math.abs(root.scrollTop - lastScroll) < 1) return;
    lastScroll = root.scrollTop;
    if (!feed.loading && !feed.done && !feed.error && root.scrollHeight - root.clientHeight - root.scrollTop < 80)
      void feed.load();
  };
  const onClick = (event) => {
    if (event.target.closest('[data-feed-more]')) void feed.load();
  };
  root.addEventListener('scroll', onScroll, { passive: true });
  root.addEventListener('click', onClick);
  feed.paint = paint;
  feed.reveal = (el) => {
    el.scrollIntoView({ block: 'nearest' });
    lastScroll = root.scrollTop;
  };
  feed.dispose = () => {
    feed.disposed = true;
    feed.epoch++;
    feed.rows = [];
    feed.data = null;
    feed.cursor = null;
    feed.loading = false;
    workingRows?.splice(0);
    workingRows = null;
    keys.clear();
    pending = null;
    refreshQueued = false;
    root.removeEventListener('scroll', onScroll);
    root.removeEventListener('click', onClick);
  };
  return feed;
}
function pageFeedStatus(feed) {
  const count = `已加载${feed.rows.length}${feed.data ? '/' + feed.data.total : ''}条`;
  return `<div class="helper page-feed-status" role="status">${feed.loading ? '正在加载…' : feed.error ? `${esc(feed.error)} <button class="quiet" data-feed-more>重试</button>` : feed.done ? `${count} · 已全部加载` : `<button class="quiet" data-feed-more>继续加载</button> · ${count}`}</div>`;
}
let readerPage = '',
  readerEpoch = 0;
function syncReaderPage() {
  if (readerPage === page) return;
  const previous = readerPage;
  readerPage = page;
  readerEpoch++;
  if (previous === 'inbox') releaseInbox();
  if (previous === 'logs') releaseLogs();
  if (previous === 'reports') releaseReports();
  if (previous === 'prompts') releasePromptHistory();
  if (previous === 'tasks' && taskFeed) releaseTaskFeed();
  if (previous && modalFromTrace) closeModal();
}
