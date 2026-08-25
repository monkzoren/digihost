// DigiHost web interface.
//
// Three jobs: purely local view state (tab, platform filter, selection, which
// nav accordion is open), swapping in fresh table markup pushed from the
// server over SSE, and posting operator actions. Nothing here fetches on a
// timer — the server says when the fleet changed.

(() => {
  const state = {
    tab: 'servers',
    platform: 'all',
    selected: null,
    // Deployment whose log drawer is open, if any.
    openLog: null,
  };

  const $ = (sel, root = document) => root.querySelector(sel);
  const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

  // ------------------------------------------------------------------ nav

  function wireNav() {
    // Accordion sections only; items are real links (or inert 'soon' stubs),
    // and the server marks the active one.
    $$('.nav-section').forEach((btn) => {
      btn.addEventListener('click', () => {
        const group = btn.closest('.nav-group');
        const wasOpen = group.classList.contains('open');
        $$('.nav-group').forEach((g) => g.classList.remove('open'));
        if (!wasOpen) group.classList.add('open');
      });
    });
  }

  // --------------------------------------------------------------- filters

  function applyView() {
    $$('.tab').forEach((t) => t.classList.toggle('active', t.dataset.tab === state.tab));
    $$('[data-panel]').forEach((p) => p.classList.toggle('hidden', p.dataset.panel !== state.tab));
    $$('.chip').forEach((c) => c.classList.toggle('active', c.dataset.platform === state.platform));

    let shown = 0;
    $$(`[data-panel="${state.tab}"] .trow`).forEach((row) => {
      const match = state.platform === 'all' || row.dataset.platform === state.platform;
      row.classList.toggle('hidden', !match);
      row.classList.toggle('selected', match && row.dataset.id === state.selected);
      if (match) shown += 1;
    });

    const empty = $(`[data-panel="${state.tab}"] .empty`);
    if (empty) {
      const title = $('.empty-title', empty);
      const body = $('.empty-body', empty);
      // Remember what the server rendered, so clearing a filter puts the real
      // "nothing here yet" message back instead of stranding the filter one.
      if (!empty.dataset.defaultTitle) {
        empty.dataset.defaultTitle = title.textContent;
        empty.dataset.defaultBody = body.textContent;
      }

      const hasRows = $$(`[data-panel="${state.tab}"] .trow`).length > 0;
      const filteredOut = hasRows && shown === 0;
      empty.classList.toggle('hidden', hasRows && shown > 0);

      if (filteredOut) {
        title.textContent = 'Nothing matches this filter';
        body.textContent = `No ${state.platform} ${
          state.tab === 'servers' ? 'hosts' : 'deployments'
        } right now.`;
      } else {
        title.textContent = empty.dataset.defaultTitle;
        body.textContent = empty.dataset.defaultBody;
      }
    }

    updateCounts();
  }

  // Counts follow the visible tab, so "Linux 4" always means four of the
  // things currently on screen.
  function updateCounts() {
    const rows = $$(`[data-panel="${state.tab}"] .trow`);
    const tally = (p) => rows.filter((r) => p === 'all' || r.dataset.platform === p).length;
    $$('.chip').forEach((chip) => {
      const slot = $('[data-count]', chip);
      if (slot) slot.textContent = tally(chip.dataset.platform);
    });
  }

  function wireControls() {
    $$('.tab').forEach((tab) => {
      if (tab.dataset.wired) return;
      tab.dataset.wired = '1';
      tab.addEventListener('click', () => {
        state.tab = tab.dataset.tab;
        applyView();
      });
    });
    $$('.chip').forEach((chip) => {
      if (chip.dataset.wired) return;
      chip.dataset.wired = '1';
      chip.addEventListener('click', () => {
        state.platform = chip.dataset.platform;
        applyView();
      });
    });
  }

  function wireRows(root = document) {
    $$('.trow', root).forEach((row) => {
      if (row.dataset.wired) return;
      row.dataset.wired = '1';
      row.addEventListener('click', () => {
        state.selected = state.selected === row.dataset.id ? null : row.dataset.id;
        applyView();

        // Deployment ids are prefixed 'd' to keep them distinct from host ids.
        const id = row.dataset.id || '';
        if (id.startsWith('d')) showLog(id.slice(1));
      });
    });
  }

  // --------------------------------------------------------------- actions

  function toast(message, kind) {
    $$('.toast').forEach((t) => t.remove());
    const el = document.createElement('div');
    el.className = `toast ${kind}`;
    el.textContent = message;
    document.body.appendChild(el);
    setTimeout(() => el.remove(), kind === 'bad' ? 8000 : 4000);
  }

  // Collect a dialog's inputs, plus any data-field-* on the button itself, so
  // row buttons and dialog forms post through one code path.
  function collectFields(button) {
    const body = new URLSearchParams();
    const scope = button.closest('dialog') || button.closest('form');
    if (scope) {
      $$('input[name], select[name], textarea[name]', scope).forEach((el) => {
        if (el.type === 'checkbox' && !el.checked) return;
        body.set(el.name, el.value);
      });
    }
    Object.entries(button.dataset).forEach(([key, value]) => {
      if (key.startsWith('field')) {
        // data-field-host_id -> host_id
        const name = key.slice('field'.length).replace(/^[A-Z]/, (c) => c.toLowerCase());
        body.set(name, value);
      }
    });
    return body;
  }

  async function runAction(button) {
    // Destructive buttons say what they are about to do and wait for a yes.
    if (button.dataset.confirm && !window.confirm(button.dataset.confirm)) return;
    const url = button.dataset.action;
    const body = collectFields(button);
    const previous = button.textContent;
    button.disabled = true;

    try {
      const res = await fetch(url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        body,
        redirect: 'follow',
      });

      const text = await res.text();
      if (!res.ok) {
        toast(text || `${res.status} ${res.statusText}`, 'bad');
        return;
      }

      if (button.dataset.navigate) {
        window.location.href = button.dataset.navigate;
        return;
      }

      // An action that reports back (minting an enrolment code) shows its
      // result in place rather than closing the dialog out from under it.
      const target = button.dataset.result && $(button.dataset.result);
      if (target) {
        let shown = text;
        try {
          const parsed = JSON.parse(text);
          shown = parsed.command || text;
        } catch {
          /* not JSON; show it raw */
        }
        target.textContent = shown;
        target.classList.remove('hidden');
        return;
      }

      button.closest('dialog')?.close();
      // The SSE stream repaints the fleet; nothing to reload.
      toast('Done', 'ok');
    } catch (err) {
      toast(String(err), 'bad');
    } finally {
      button.disabled = false;
      button.textContent = previous;
    }
  }

  function wireActions(root = document) {
    $$('[data-open]', root).forEach((btn) => {
      if (btn.dataset.wired) return;
      btn.dataset.wired = '1';
      btn.addEventListener('click', () => {
        document.getElementById(btn.dataset.open)?.showModal();
      });
    });

    $$('[data-close]', root).forEach((btn) => {
      if (btn.dataset.wired) return;
      btn.dataset.wired = '1';
      btn.addEventListener('click', () => btn.closest('dialog')?.close());
    });

    // "Reset password" buttons open one shared dialog aimed at their user.
    $$('[data-reset-user]', root).forEach((btn) => {
      if (btn.dataset.wired) return;
      btn.dataset.wired = '1';
      btn.addEventListener('click', () => {
        const dlg = document.getElementById('dlg-reset');
        if (!dlg) return;
        dlg.querySelector('[name="user"]').value = btn.dataset.resetUser;
        const label = dlg.querySelector('[data-reset-label]');
        if (label) label.textContent = btn.dataset.resetUser;
        dlg.showModal();
      });
    });

    $$('[data-detect]', root).forEach((btn) => {
      if (btn.dataset.wired) return;
      btn.dataset.wired = '1';
      btn.addEventListener('click', (e) => {
        e.preventDefault();
        inspectRepo(btn);
      });
    });

    $$('[data-action]:not([data-detect])', root).forEach((btn) => {
      if (btn.dataset.wired) return;
      btn.dataset.wired = '1';
      btn.addEventListener('click', (e) => {
        // Row action buttons live inside a clickable row; a click here means
        // the action, not "select this row".
        e.stopPropagation();
        runAction(btn);
      });
    });
  }

  // --------------------------------------------------------------- guided

  function setField(name, value) {
    const el = $(`#dlg-app [name="${name}"]`);
    if (el && value !== null && value !== undefined && value !== '') el.value = value;
  }

  // Fill in what detection worked out, and show why, so the operator can
  // judge the guess rather than take it on faith.
  function applyDetection(detected) {
    const box = $('#detect-result');
    if (!box) return;

    box.classList.remove('hidden');
    box.classList.toggle('unsure', !detected.confident);
    box.textContent = detected.confident
      ? `Looks like ${detected.strategy} — ${detected.because}.`
      : `Not sure: ${detected.because}. Proposing ${detected.strategy}.`;

    setField('strategy', detected.strategy);
    setField('entrypoint', detected.entrypoint);
    setField('port', detected.port);

    // The repository's own name is the obvious application name; retyping it
    // is the sort of thing that makes people resent a form.
    const repo = $('#dlg-app [name="repo"]')?.value || '';
    // Application names are slugs (they feed compose project names), so the
    // suggestion is slugified up front rather than bounced by the server.
    const suggested = (repo.split('/').pop() || '')
      .toLowerCase()
      .replace(/[^a-z0-9-]+/g, '-')
      .replace(/^-+|-+$/g, '');
    const nameField = $('#dlg-app [name="name"]');
    if (nameField && !nameField.value && suggested) nameField.value = suggested;
  }

  async function inspectRepo(button) {
    const repo = $('#dlg-app [name="repo"]')?.value?.trim();
    if (!repo) {
      toast('Enter a repository first', 'bad');
      return;
    }

    const label = button.textContent;
    button.disabled = true;
    button.textContent = 'Inspecting…';
    try {
      const body = new URLSearchParams({
        repo,
        branch: $('#dlg-app [name="branch"]')?.value || '',
        visibility: $('#dlg-app [name="visibility"]')?.value || 'public',
      });
      const res = await fetch('/actions/detect', {
        method: 'POST',
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        body,
      });
      const text = await res.text();
      if (!res.ok) {
        toast(text, 'bad');
        return;
      }
      applyDetection(JSON.parse(text));
    } catch (err) {
      toast(String(err), 'bad');
    } finally {
      button.disabled = false;
      button.textContent = label;
    }
  }

  async function loadRepoPicker() {
    const picker = $('[data-repo-picker]');
    if (!picker) return;

    try {
      const res = await fetch('/actions/repos');
      if (!res.ok) {
        picker.innerHTML = '<option value="">Could not list repositories</option>';
        return;
      }
      const repos = await res.json();
      picker.innerHTML =
        '<option value="">Choose a repository…</option>' +
        repos
          .map((r) => {
            const name = r.full_name.replace(/[<>&"]/g, '');
            const tag = r.private ? ' (private)' : '';
            return `<option value="${name}" data-branch="${r.default_branch || ''}" data-private="${r.private}">${name}${tag}</option>`;
          })
          .join('');

      picker.addEventListener('change', () => {
        const opt = picker.selectedOptions[0];
        if (!opt || !opt.value) return;
        setField('repo', opt.value);
        setField('branch', opt.dataset.branch);
        const visibility = $('#dlg-app [name="visibility"]');
        if (visibility) visibility.value = opt.dataset.private === 'true' ? 'private' : 'public';
        // Picking a repository is a clear signal to go and look at it.
        const inspect = $('#dlg-app [data-detect]');
        if (inspect) inspectRepo(inspect);
      });
    } catch {
      picker.innerHTML = '<option value="">Could not list repositories</option>';
    }
  }

  // The deploy dialog offers the strategy each application actually uses.
  function wireStrategyFollow() {
    const apps = $('[data-app-picker]');
    const strategy = $('[data-strategy-picker]');
    if (!apps || !strategy) return;

    const follow = () => {
      const chosen = apps.selectedOptions[0]?.dataset.strategy;
      if (chosen) strategy.value = chosen;
    };
    apps.addEventListener('change', follow);
    follow();
  }

  // ---------------------------------------------------------------- drawer

  function isLogAtBottom() {
    const log = $('#drawer .log');
    if (!log) return true;
    return log.scrollHeight - log.scrollTop - log.clientHeight < 40;
  }

  async function showLog(deploymentId) {
    const drawer = $('#drawer');
    if (!drawer) return;

    try {
      const res = await fetch(`/deployments/${deploymentId}/log`);
      if (!res.ok) {
        toast(await res.text(), 'bad');
        return;
      }
      const wasAtBottom = isLogAtBottom();
      drawer.innerHTML = await res.text();
      drawer.classList.remove('hidden');
      state.openLog = deploymentId;

      $$('[data-close-drawer]', drawer).forEach((btn) =>
        btn.addEventListener('click', closeLog),
      );

      // Follow the tail only if the reader had not scrolled up to read
      // something — yanking them back mid-read is worse than not following.
      const log = $('.log', drawer);
      if (log && wasAtBottom) log.scrollTop = log.scrollHeight;
    } catch (err) {
      toast(String(err), 'bad');
    }
  }

  function closeLog() {
    state.openLog = null;
    const drawer = $('#drawer');
    if (drawer) {
      drawer.classList.add('hidden');
      drawer.innerHTML = '';
    }
  }

  // ------------------------------------------------------------------- SSE

  function connect() {
    const live = $('.live');
    if (!$('#fleet')) return; // pages without live content skip the stream

    const source = new EventSource('/events');

    source.addEventListener('fleet', (e) => {
      const payload = JSON.parse(e.data);
      const main = $('#fleet');
      if (!main) return;
      // The server owns rendering; the client re-binds and re-applies view
      // state on the fresh markup.
      main.innerHTML = payload.html;
      wireRows(main);
      wireControls();
      wireActions(main);
      applyView();

      const summary = $('.fleet-summary');
      if (summary) summary.textContent = payload.summary;
      if (live) {
        live.classList.remove('stale');
        $('.live-text', live).textContent = 'Live';
      }
      if (state.openLog !== null) showLog(state.openLog);
    });

    source.addEventListener('error', () => {
      if (live) {
        live.classList.add('stale');
        $('.live-text', live).textContent = 'Reconnecting…';
      }
    });
  }

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && state.openLog !== null) closeLog();
  });

  wireNav();
  wireControls();
  wireRows();
  wireActions();
  wireStrategyFollow();
  loadRepoPicker();
  applyView();
  connect();
})();
