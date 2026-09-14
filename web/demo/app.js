/* chaoticdb web demo console - zero dependency, no build step.
 * All server-derived strings are inserted via textContent / DOM APIs. No innerHTML, no eval. */
'use strict';

(function () {
  /* ------------------------------------------------------------------ *
   * tiny DOM helpers                                                    *
   * ------------------------------------------------------------------ */
  function $(sel, root) { return (root || document).querySelector(sel); }

  function el(tag, opts, children) {
    var node = document.createElement(tag);
    opts = opts || {};
    if (opts.className) node.className = opts.className;
    if (opts.text != null) node.textContent = String(opts.text);
    if (opts.title != null) node.title = String(opts.title);
    if (opts.attrs) {
      Object.keys(opts.attrs).forEach(function (k) {
        if (opts.attrs[k] != null) node.setAttribute(k, String(opts.attrs[k]));
      });
    }
    if (opts.dataset) {
      Object.keys(opts.dataset).forEach(function (k) { node.dataset[k] = String(opts.dataset[k]); });
    }
    if (opts.on) {
      Object.keys(opts.on).forEach(function (k) { node.addEventListener(k, opts.on[k]); });
    }
    appendChildren(node, children);
    return node;
  }

  function appendChildren(node, children) {
    if (children == null) return;
    var list = Array.isArray(children) ? children : [children];
    list.forEach(function (child) {
      if (child == null) return;
      node.append(child.nodeType ? child : document.createTextNode(String(child)));
    });
  }

  function clear(node) {
    while (node.firstChild) node.removeChild(node.firstChild);
    return node;
  }

  function pre(text, className) {
    return el('pre', { className: className || 'code', text: text == null ? '' : text });
  }

  function makeCard(title, body, extraHead) {
    var card = el('div', { className: 'card card-flush' });
    var head = el('div', { className: 'card-head' }, [el('span', { className: 'card-title', text: title })]);
    if (extraHead) head.append(extraHead);
    card.append(head);
    if (body) card.append(body);
    return card;
  }

  function badge(text, className) {
    return el('span', { className: 'badge ' + (className || ''), text: text });
  }

  function fmtBytes(n) {
    if (typeof n !== 'number' || !isFinite(n)) return String(n == null ? '' : n);
    if (n < 1024) return n + ' B';
    var units = ['KB', 'MB', 'GB', 'TB'];
    var v = n / 1024;
    var i = 0;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
    return v.toFixed(v >= 10 ? 1 : 2) + ' ' + units[i];
  }

  function formatCell(v) {
    if (v === null || v === undefined) return { text: 'NULL', isNull: true };
    if (typeof v === 'boolean') return { text: v ? 'true' : 'false', isNum: false };
    if (typeof v === 'number') return { text: String(v), isNum: true };
    return { text: String(v), isNum: false };
  }

  /* ------------------------------------------------------------------ *
   * api helper (session aware)                                          *
   * ------------------------------------------------------------------ */
  var sessionId = null;

  function request(method, path, body) {
    var headers = {};
    if (sessionId) headers['X-Chibi-Session'] = sessionId;
    var init = { method: method, headers: headers };
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
    }
    return fetch(path, init).then(function (res) {
      return res.text().then(function (raw) {
        var data = null;
        if (raw) { try { data = JSON.parse(raw); } catch (e) { data = null; } }
        if (!res.ok) {
          var msg = data && data.error != null ? String(data.error) : 'HTTP ' + res.status;
          var err = new Error(msg);
          err.status = res.status;
          err.data = data;
          throw err;
        }
        return data;
      });
    });
  }

  function fetchSession() {
    return fetch('/session').then(function (res) {
      if (!res.ok) return null;
      var header = res.headers.get('X-Chibi-Session');
      return res.json().then(function (d) {
        return header || (d && d.session) || null;
      }).catch(function () { return header; });
    }).catch(function () { return null; });
  }

  /* ------------------------------------------------------------------ *
   * state + tabs                                                        *
   * ------------------------------------------------------------------ */
  var state = {
    config: null,
    schema: null,
    session: null,
    storage: { db: null, file: null, page: 0, total: 0, files: [] }
  };
  var storageTabDisabled = false;

  function activateTab(name) {
    var target = $('.tab[data-tab="' + name + '"]');
    if (!target || target.disabled) return;
    Array.prototype.forEach.call(document.querySelectorAll('.tab'), function (t) {
      var on = t === target;
      t.classList.toggle('is-active', on);
      t.setAttribute('aria-selected', on ? 'true' : 'false');
    });
    Array.prototype.forEach.call(document.querySelectorAll('.panel'), function (p) {
      var on = p.id === 'panel-' + name;
      p.classList.toggle('is-active', on);
      p.hidden = !on;
    });
  }

  function disableStorageTab() {
    storageTabDisabled = true;
    var tab = $('.tab[data-tab="storage"]');
    if (tab) {
      tab.disabled = true;
      tab.classList.add('is-disabled');
      tab.title = 'storage preview disabled (set [web] page_preview = true)';
    }
    var panel = $('#panel-storage');
    if (panel && panel.classList.contains('is-active')) activateTab('sql');
  }

  /* ------------------------------------------------------------------ *
   * health / config                                                     *
   * ------------------------------------------------------------------ */
  function loadHealth() {
    var pill = $('#health-pill');
    return request('GET', '/health').then(function (d) {
      if (d && d.status === 'ok') {
        pill.textContent = 'ok';
        pill.className = 'pill pill-ok';
        pill.title = 'GET /health -> ok';
      } else {
        pill.textContent = '异常';
        pill.className = 'pill pill-err';
        pill.title = 'GET /health 返回非 ok';
      }
    }).catch(function () {
      pill.textContent = '未连接';
      pill.className = 'pill pill-err';
      pill.title = 'GET /health 请求失败';
    });
  }

  function loadConfig() {
    return request('GET', '/api/config').then(function (d) {
      state.config = d || {};
      if (d && d.title) {
        document.title = String(d.title);
        $('#app-title').textContent = String(d.title);
      }
      if (d && d.page_preview === false) disableStorageTab();
    }).catch(function () {
      /* keep defaults; storage will surface its own 404 notice if disabled */
    });
  }

  /* ------------------------------------------------------------------ *
   * SQL console                                                         *
   * ------------------------------------------------------------------ */
  var history = [];

  function pushHistory(sql) {
    history = history.filter(function (s) { return s !== sql; });
    history.unshift(sql);
    if (history.length > 30) history.length = 30;
    renderHistory();
  }

  function renderHistory() {
    var box = $('#sql-history');
    clear(box);
    if (!history.length) {
      box.append(el('span', { className: 'muted', text: '暂无历史' }));
      return;
    }
    history.forEach(function (sql) {
      box.append(el('button', {
        className: 'history-item',
        text: sql,
        title: sql,
        attrs: { type: 'button' },
        on: {
          click: function () {
            $('#sql-input').value = sql;
            $('#sql-input').focus();
          }
        }
      }));
    });
  }

  function renderQueryResults(container, results) {
    clear(container);
    if (!Array.isArray(results) || results.length === 0) {
      container.append(el('div', { className: 'banner banner-warn', text: '（无结果）' }));
      return;
    }
    results.forEach(function (item) {
      container.append(renderResultItem(item));
    });
  }

  function renderResultItem(item) {
    if (!item || typeof item !== 'object') {
      return makeCard('Result', el('div', { className: 'result-line', text: JSON.stringify(item) }));
    }
    if (item.type === 'rows') {
      return renderRowsResult(item);
    }
    if (item.type === 'message') {
      var card = el('div', { className: 'result-block' });
      card.append(el('div', { className: 'result-line ok', text: item.message == null ? 'SUCCESS' : String(item.message) }));
      return card;
    }
    if (item.type === 'affected') {
      var n = item.affected;
      var card2 = el('div', { className: 'result-block' });
      card2.append(el('div', { className: 'result-line affected', text: (n == null ? 0 : n) + ' rows affected' }));
      return card2;
    }
    var unknown = el('div', { className: 'result-block' });
    unknown.append(el('div', { className: 'result-head' }, [
      el('span', { text: 'unknown result type' }),
      el('span', { text: item.type == null ? '?' : String(item.type) })
    ]));
    unknown.append(pre(JSON.stringify(item, null, 2)));
    return unknown;
  }

  function renderRowsResult(item) {
    var columns = Array.isArray(item.columns) ? item.columns : [];
    var rows = Array.isArray(item.rows) ? item.rows : [];
    var block = el('div', { className: 'result-block' });
    block.append(el('div', { className: 'result-head' }, [
      el('span', { text: rows.length + ' rows' }),
      el('span', { text: columns.length + ' columns' })
    ]));

    var wrap = el('div', { className: 'table-wrap' });
    var table = el('table', { className: 'grid' });

    var thead = el('thead');
    var htr = el('tr');
    columns.forEach(function (c) { htr.append(el('th', { text: c })); });
    thead.append(htr);
    table.append(thead);

    var tbody = el('tbody');
    rows.forEach(function (row) {
      var tr = el('tr');
      var cells = Array.isArray(row) ? row : [];
      for (var i = 0; i < columns.length; i++) {
        var info = formatCell(cells[i]);
        var cls = info.isNull ? 'is-null' : (info.isNum ? 'is-num' : '');
        tr.append(el('td', { className: cls, text: info.text }));
      }
      tbody.append(tr);
    });
    table.append(tbody);
    wrap.append(table);
    block.append(wrap);
    return block;
  }

  function renderError(container, message) {
    container.append(el('div', { className: 'banner banner-error', text: '错误: ' + message }));
  }

  function runSql() {
    var input = $('#sql-input');
    var sql = input.value.trim();
    if (!sql) return;
    var status = $('#sql-status');
    var box = $('#sql-results');
    clear(box);
    status.textContent = '执行中…';
    request('POST', '/query', { sql: sql }).then(function (d) {
      pushHistory(sql);
      renderQueryResults(box, d && d.results);
      status.textContent = '';
    }).catch(function (e) {
      renderError(box, e.message);
      status.textContent = '';
    });
  }

  function initSqlPanel() {
    $('#sql-run').addEventListener('click', runSql);
    $('#sql-clear').addEventListener('click', function () { $('#sql-results') && clear($('#sql-results')); });
    $('#sql-input').addEventListener('keydown', function (e) {
      if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
        e.preventDefault();
        runSql();
      }
    });
  }

  /* ------------------------------------------------------------------ *
   * schema (shared by sidebar, storage db select, tree)                 *
   * ------------------------------------------------------------------ */
  function loadSchema() {
    return request('GET', '/api/schema').then(function (d) {
      state.schema = d || { databases: [] };
      renderSchemaMini(state.schema);
      renderSchemaTree(state.schema);
      populateDbSelect(state.schema);
      return state.schema;
    }).catch(function (e) {
      var box = $('#sql-schema');
      clear(box);
      box.append(el('span', { className: 'muted', text: 'Schema 加载失败: ' + e.message }));
      $('#schema-tree') && clear($('#schema-tree')).append(el('span', { className: 'muted', text: '加载失败: ' + e.message }));
    });
  }

  function databases() {
    return (state.schema && Array.isArray(state.schema.databases)) ? state.schema.databases : [];
  }

  function renderSchemaMini(schema) {
    var box = $('#sql-schema');
    clear(box);
    var dbs = (schema && schema.databases) || [];
    if (!dbs.length) {
      box.append(el('span', { className: 'muted', text: '（无数据库）' }));
      return;
    }
    dbs.forEach(function (db) {
      var group = el('div', { className: 'db-group' });
      group.append(el('div', { className: 'db-name', text: db.name }));
      (db.tables || []).forEach(function (t) {
        group.append(el('button', {
          className: 'tbl-btn',
          text: t.name,
          title: 'SELECT * FROM ' + t.name + ';',
          attrs: { type: 'button' },
          on: {
            click: function () {
              var input = $('#sql-input');
              input.value = 'SELECT * FROM ' + t.name + ';';
              input.focus();
            }
          }
        }));
        var ul = el('ul', { className: 'col-list' });
        (t.columns || []).forEach(function (c) {
          ul.append(el('li', { className: 'col-item' }, [
            el('span', { text: c.name }),
            el('span', { className: 'col-type', text: c.type == null ? '' : c.type })
          ]));
        });
        group.append(ul);
      });
      box.append(group);
    });
  }

  function columnBadges(c) {
    var frag = document.createDocumentFragment();
    var b = el('span', { className: 'badges' });
    if (c.primary_key) b.append(badge('PK', 'badge-pk'));
    if (c.unique) b.append(badge('UNIQUE', 'badge-unique'));
    if (c.not_null) b.append(badge('NOT NULL', 'badge-notnull'));
    if (c.default != null && c.default !== '') b.append(badge('DEFAULT ' + c.default, 'badge-default'));
    frag.append(b);
    return frag;
  }

  function renderSchemaTree(schema) {
    var box = $('#schema-tree');
    if (!box) return;
    clear(box);
    var dbs = (schema && schema.databases) || [];
    var tableCount = 0;
    if (!dbs.length) {
      box.append(el('span', { className: 'muted', text: '（无数据库）' }));
      return;
    }
    dbs.forEach(function (db) {
      var dbBlock = el('div', { className: 'db-block' });
      dbBlock.append(el('div', { className: 'db-title' }, [
        el('span', { text: db.name }),
        badge((db.tables || []).length + ' tables')
      ]));
      (db.tables || []).forEach(function (t) {
        tableCount++;
        var tbl = el('div', { className: 'tbl-block' });
        tbl.append(el('div', { className: 'tbl-title' }, [
          el('span', { className: 't-name', text: t.name }),
          t.engine ? badge(t.engine, 'badge-k') : null,
          t.layout ? badge(t.layout) : null
        ]));

        var table = el('table', { className: 'col-table' });
        var thead = el('thead');
        thead.append(el('tr', {}, [
          el('th', { text: 'column' }),
          el('th', { text: 'type' }),
          el('th', { text: 'constraints' })
        ]));
        table.append(thead);
        var tbody = el('tbody');
        (t.columns || []).forEach(function (c) {
          var tr = el('tr');
          tr.append(el('td', { className: 'c-name', text: c.name }));
          tr.append(el('td', { className: 'c-type', text: c.type == null ? '' : c.type }));
          var td = el('td');
          td.append(columnBadges(c));
          tr.append(td);
          tbody.append(tr);
        });
        table.append(tbody);
        tbl.append(table);
        dbBlock.append(tbl);
      });
      box.append(dbBlock);
    });
    var summary = $('#schema-summary');
    if (summary) summary.textContent = dbs.length + ' databases · ' + tableCount + ' tables';
  }

  function populateDbSelect(schema) {
    var sel = $('#storage-db');
    if (!sel) return;
    var dbs = (schema && schema.databases) || [];
    clear(sel);
    if (!dbs.length) {
      sel.append(el('option', { text: '（无数据库）', attrs: { value: '' } }));
      return;
    }
    dbs.forEach(function (db) {
      sel.append(el('option', { text: db.name, attrs: { value: db.name } }));
    });
    state.storage.db = dbs[0].name;
    sel.value = dbs[0].name;
  }

  /* ------------------------------------------------------------------ *
   * pipeline panel                                                      *
   * ------------------------------------------------------------------ */
  function runPlan() {
    var sql = $('#plan-input').value.trim();
    if (!sql) return;
    var status = $('#plan-status');
    status.textContent = '编译中…';
    request('POST', '/api/plan', { sql: sql }).then(function (d) {
      renderPlanResult(d || {});
      status.textContent = '';
    }).catch(function (e) {
      renderPlanResult({ error: { stage: 'request', message: e.message, pos: null }, sql: sql });
      status.textContent = '';
    });
  }

  function renderPlanResult(d) {
    renderTokens(Array.isArray(d.tokens) ? d.tokens : []);
    renderStatements(Array.isArray(d.statements) ? d.statements : []);
    renderPreBlock('#pipe-plan', d.plan);
    renderPreBlock('#pipe-physical', d.physical);
    renderPlanError(d.error, typeof d.sql === 'string' ? d.sql : $('#plan-input').value);
  }

  function renderPreBlock(sel, value) {
    var box = $(sel);
    clear(box);
    if (value == null || value === '') {
      box.append(el('span', { className: 'muted', text: '—' }));
    } else {
      box.append(pre(String(value)));
    }
  }

  function renderStatements(statements) {
    var box = $('#pipe-statement');
    clear(box);
    if (!statements.length) {
      box.append(el('span', { className: 'muted', text: '—' }));
      return;
    }
    statements.forEach(function (st, i) {
      if (i > 0) box.append(el('div', { className: 'muted', text: '── statement ' + (i + 1) + ' ──' }));
      box.append(pre(st && st.debug != null ? String(st.debug) : '(no debug)'));
    });
  }

  function renderTokens(tokens) {
    var box = $('#pipe-tokens');
    var detail = $('#pipe-token-detail');
    clear(box);
    if (detail) detail.textContent = '';
    if (!tokens.length) {
      box.append(el('span', { className: 'muted', text: '—' }));
      return;
    }
    var wrap = el('div', { className: 'token-wrap' });
    tokens.forEach(function (t) {
      var kind = t && t.kind != null ? String(t.kind) : '?';
      var text = t && t.text != null ? String(t.text) : '';
      var pos = t && t.pos != null ? t.pos : null;
      var chip = el('button', {
        className: 'token-chip tk-' + kind.toLowerCase(),
        title: kind + ' · pos ' + (pos == null ? '?' : pos) + '\n' + text,
        attrs: { type: 'button' },
        on: {
          click: function () {
            if (detail) detail.textContent = 'kind=' + kind + '  pos=' + (pos == null ? 'null' : pos) + '  text=' + JSON.stringify(text);
          }
        }
      }, [
        el('span', { className: 'tk-kind', text: kind }),
        el('span', { className: 'tk-text', text: text === '' ? '∅' : text }),
        pos == null ? null : el('sup', { className: 'tk-pos', text: pos })
      ]);
      wrap.append(chip);
    });
    box.append(wrap);
  }

  function charDisplayWidth(ch) {
    var cp = ch.codePointAt(0);
    if (cp == null) return 1;
    if (
      (cp >= 0x1100 && cp <= 0x115f) ||
      (cp >= 0x2e80 && cp <= 0xa4cf) ||
      (cp >= 0xac00 && cp <= 0xd7a3) ||
      (cp >= 0xf900 && cp <= 0xfaff) ||
      (cp >= 0xfe30 && cp <= 0xfe4f) ||
      (cp >= 0xff00 && cp <= 0xff60) ||
      (cp >= 0xffe0 && cp <= 0xffe6) ||
      (cp >= 0x1f300 && cp <= 0x1faff)
    ) return 2;
    return 1;
  }

  function caretPrefix(sql, bytePos) {
    var bytes = 0;
    var width = 0;
    var chars = Array.from(String(sql));
    for (var i = 0; i < chars.length; i++) {
      var ch = chars[i];
      var len = new TextEncoder().encode(ch).length;
      if (bytes + len > bytePos) break;
      bytes += len;
      width += charDisplayWidth(ch);
    }
    return ' '.repeat(Math.max(0, width));
  }

  function renderPlanError(error, sql) {
    var box = $('#plan-error');
    clear(box);
    ['tokens', 'statement', 'plan', 'physical'].forEach(function (n) {
      var card = $('#pipe-card-' + n);
      if (card) card.classList.remove('stage-error');
    });
    if (!error) return;

    var stage = String(error.stage == null ? 'unknown' : error.stage);
    var map = { lex: 'tokens', parse: 'statement', plan: 'plan' };
    var cardId = map[stage] ? '#pipe-card-' + map[stage] : null;
    if (cardId) {
      var c = $(cardId);
      if (c) {
        c.classList.add('stage-error');
        var collapsed = c.classList.contains('collapsed');
        if (collapsed) c.classList.remove('collapsed');
      }
    }

    var banner = el('div', { className: 'banner banner-error' });
    banner.append(el('div', { className: 'result-head', text: '编译错误 · ' + stage + ' 阶段' }));
    banner.append(el('div', { text: String(error.message == null ? '' : error.message) }));
    if (error.pos != null && sql) {
      var caret = caretPrefix(sql, Number(error.pos)) + '^ pos=' + error.pos;
      banner.append(pre(caret, 'code caret-pre'));
    }
    box.append(banner);
  }

  function initPipelinePanel() {
    $('#plan-run').addEventListener('click', runPlan);
    $('#plan-input').addEventListener('keydown', function (e) {
      if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
        e.preventDefault();
        runPlan();
      }
    });
    Array.prototype.forEach.call(document.querySelectorAll('.collapsible'), function (head) {
      head.addEventListener('click', function () {
        var target = document.getElementById(head.dataset.collapse);
        if (!target) return;
        target.closest('.pipe-card').classList.toggle('collapsed');
      });
    });
  }

  /* ------------------------------------------------------------------ *
   * storage panel                                                       *
   * ------------------------------------------------------------------ */
  function kindClass(kind) {
    if (['heap', 'index', 'lsm', 'wal', 'catalog', 'lob'].indexOf(kind) >= 0) return 'badge-' + kind;
    return 'badge-k';
  }

  function showStorageDisabled() {
    var notice = $('#storage-disabled');
    if (notice) notice.hidden = false;
    var viewer = $('#storage-viewer');
    if (viewer) viewer.hidden = true;
    disableStorageTab();
  }

  function loadStorageFiles() {
    var list = $('#storage-files');
    var db = state.storage.db;
    if (!db) {
      clear(list);
      list.append(el('span', { className: 'muted', text: '（无数据库）' }));
      return;
    }
    clear(list);
    list.append(el('span', { className: 'muted', text: '加载中…' }));
    request('GET', '/api/files?db=' + encodeURIComponent(db)).then(function (d) {
      state.storage.files = (d && d.files) || [];
      $('#storage-disabled').hidden = true;
      $('#storage-viewer').hidden = false;
      renderFileList(state.storage.files);
      if (state.storage.files.length) {
        selectFile(state.storage.files[0].name, 0);
      } else {
        clear($('#page-meta'));
        clear($('#page-hex'));
      }
    }).catch(function (e) {
      if (e.status === 404) {
        showStorageDisabled();
      } else {
        clear(list);
        list.append(el('div', { className: 'banner banner-error', text: e.message }));
      }
    });
  }

  function renderFileList(files) {
    var list = $('#storage-files');
    clear(list);
    if (!files.length) {
      list.append(el('span', { className: 'muted', text: '（无文件）' }));
      return;
    }
    files.forEach(function (f) {
      var kind = f.kind == null ? 'other' : String(f.kind);
      var item = el('button', {
        className: 'file-item',
        dataset: { file: f.name },
        title: f.name,
        attrs: { type: 'button' },
        on: {
          click: function () { selectFile(f.name, 0); }
        }
      }, [
        el('span', { className: 'f-name', text: f.name }),
        el('span', { className: 'f-meta' }, [
          badge(kind, kindClass(kind)),
          el('span', { text: fmtBytes(f.size) }),
          el('span', { text: (f.pages == null ? '?' : f.pages) + ' pages' })
        ])
      ]);
      list.append(item);
    });
  }

  function selectFile(name, pageNo) {
    state.storage.file = name;
    Array.prototype.forEach.call(document.querySelectorAll('.file-item'), function (b) {
      b.classList.toggle('is-active', b.dataset.file === name);
    });
    loadPage(pageNo || 0);
  }

  function setPageNav(no, total) {
    var prev = $('#page-prev');
    var next = $('#page-next');
    var indicator = $('#page-indicator');
    var jump = $('#page-jump');
    prev.disabled = no <= 0;
    next.disabled = total <= 0 || no >= total - 1;
    indicator.textContent = no + ' / ' + total;
    if (jump) {
      jump.max = Math.max(0, total - 1);
      jump.value = no;
    }
  }

  function loadPage(no) {
    if (!state.storage.file) return;
    var meta = $('#page-meta');
    var hex = $('#page-hex');
    clear(meta);
    clear(hex);
    meta.append(el('div', { className: 'muted', text: '加载中…' }));
    var url = '/api/page?db=' + encodeURIComponent(state.storage.db) +
      '&file=' + encodeURIComponent(state.storage.file) +
      '&no=' + encodeURIComponent(no);
    request('GET', url).then(function (d) {
      state.storage.page = d && d.no != null ? d.no : no;
      state.storage.total = d && d.total_pages != null ? d.total_pages : 0;
      renderPage(d || {});
    }).catch(function (e) {
      clear(meta);
      if (e.status === 404) {
        showStorageDisabled();
      } else {
        meta.append(el('div', { className: 'banner banner-error', text: e.message }));
      }
    });
  }

  function renderPage(d) {
    setPageNav(d.no == null ? 0 : d.no, d.total_pages == null ? 0 : d.total_pages);
    var fileEl = $('#page-file');
    if (fileEl) fileEl.textContent = (d.file || '') + (d.kind ? ' · ' + d.kind : '') +
      (d.page_size ? ' · ' + d.page_size + ' B/page' : '');

    var meta = $('#page-meta');
    clear(meta);
    meta.append(makeCard('页面头 (header)', renderStructure(d.header)));
    meta.append(makeCard('结构 (structure)', renderStructure(d.structure)));

    var hexWrap = $('#page-hex');
    clear(hexWrap);
    if (d.notes) hexWrap.append(el('div', { className: 'muted', text: String(d.notes) }));
    hexWrap.append(renderHexDump(d.hex, d.ascii));
  }

  function renderStructure(structure) {
    if (!structure || typeof structure !== 'object') {
      return el('div', { className: 'pad muted', text: '—' });
    }
    var frag = document.createDocumentFragment();
    var dl = el('dl', { className: 'kv' });
    function add(k, v) {
      dl.append(el('dt', { text: k }));
      dl.append(el('dd', { text: v == null ? 'null' : String(v) }));
    }
    switch (structure.type) {
      case 'file_header':
        add('magic', structure.magic);
        add('format_version', structure.format_version);
        add('file_kind', structure.file_kind);
        add('page_size', structure.page_size);
        break;
      case 'slotted':
        add('num_slots', structure.num_slots);
        add('slot_end', structure.slot_end);
        add('free_bytes', structure.free_bytes);
        break;
      case 'btree_node':
        add('node_type', structure.node_type);
        add('entries', structure.entries);
        add('prev', structure.prev);
        add('next', structure.next);
        add('high_key_len', structure.high_key_len);
        break;
      case 'pax':
        add('num_slots', structure.num_slots);
        break;
      case 'wal':
        add('frames', structure.frames);
        break;
      case 'unknown':
        add('type', 'unknown');
        break;
      default:
        Object.keys(structure).forEach(function (k) {
          var v = structure[k];
          add(k, v && typeof v === 'object' ? JSON.stringify(v) : v);
        });
    }
    frag.append(dl);
    if (structure.type === 'slotted' && Array.isArray(structure.slots)) {
      frag.append(renderSlots(structure.slots));
    }
    return frag;
  }

  function renderSlots(slots) {
    var wrap = el('div', { className: 'table-wrap' });
    var table = el('table', { className: 'slot-table' });
    var thead = el('thead');
    thead.append(el('tr', {}, [
      el('th', { text: 'slot' }),
      el('th', { text: 'offset' }),
      el('th', { text: 'length' }),
      el('th', { text: 'status' })
    ]));
    table.append(thead);
    var tbody = el('tbody');
    slots.forEach(function (s) {
      var status = s && s.status != null ? String(s.status) : '';
      tbody.append(el('tr', {}, [
        el('td', { text: s && s.slot != null ? s.slot : '' }),
        el('td', { text: s && s.offset != null ? s.offset : '' }),
        el('td', { text: s && s.length != null ? s.length : '' }),
        el('td', { className: 'slot-status-' + status, text: status })
      ]));
    });
    table.append(tbody);
    wrap.append(table);
    return wrap;
  }

  function renderHexDump(hex, ascii) {
    var wrap = el('div', { className: 'hexdump' });
    var hexLines = String(hex == null ? '' : hex).split('\n');
    var asciiLines = String(ascii == null ? '' : ascii).split('\n');
    var total = Math.max(hexLines.length, asciiLines.length);
    var wrote = false;
    for (var i = 0; i < total; i++) {
      var raw = (hexLines[i] || '').trim();
      var asciiLine = asciiLines[i] == null ? '' : asciiLines[i];
      if (raw === '' && asciiLine === '') continue;
      wrote = true;
      var bytes = raw === '' ? [] : raw.split(/\s+/);
      var line = el('div', { className: 'hex-line' });
      line.append(el('span', { className: 'hex-off', text: (i * 16).toString(16).padStart(8, '0') }));
      var hexSpan = el('span', { className: 'hex-bytes' });
      for (var j = 0; j < 16; j++) {
        var b = bytes[j];
        hexSpan.append(el('span', {
          className: 'hex-byte' + (b ? '' : ' is-empty'),
          text: b ? b.padStart(2, '0') : '  '
        }));
      }
      line.append(hexSpan);
      line.append(el('span', { className: 'hex-sep', text: '|' }));
      if (asciiLine.length < 16) asciiLine += ' '.repeat(16 - asciiLine.length);
      line.append(el('span', { className: 'hex-ascii', text: asciiLine }));
      line.append(el('span', { className: 'hex-sep', text: '|' }));
      wrap.append(line);
    }
    if (!wrote) wrap.append(el('span', { className: 'muted', text: '（无数据）' }));
    return wrap;
  }

  function initStoragePanel() {
    $('#storage-db').addEventListener('change', function (e) {
      state.storage.db = e.target.value;
      state.storage.file = null;
      loadStorageFiles();
    });
    $('#page-prev').addEventListener('click', function () {
      if (state.storage.page > 0) loadPage(state.storage.page - 1);
    });
    $('#page-next').addEventListener('click', function () {
      if (state.storage.page < state.storage.total - 1) loadPage(state.storage.page + 1);
    });
    $('#page-jump-form').addEventListener('submit', function (e) {
      e.preventDefault();
      var val = parseInt($('#page-jump').value, 10);
      if (!isNaN(val)) {
        var clamped = Math.max(0, Math.min(val, Math.max(0, state.storage.total - 1)));
        loadPage(clamped);
      }
    });
  }

  /* ------------------------------------------------------------------ *
   * boot                                                                *
   * ------------------------------------------------------------------ */
  function initTabs() {
    Array.prototype.forEach.call(document.querySelectorAll('.tab'), function (tab) {
      tab.addEventListener('click', function () { activateTab(tab.dataset.tab); });
    });
  }

  function init() {
    initTabs();
    initSqlPanel();
    initPipelinePanel();
    initStoragePanel();

    fetchSession().then(function (id) {
      sessionId = id;
      var pill = $('#session-pill');
      if (id) {
        pill.textContent = id.length > 18 ? id.slice(0, 18) + '…' : id;
        pill.title = 'X-Chibi-Session: ' + id;
        pill.className = 'pill pill-muted';
        state.session = id;
      } else {
        pill.textContent = 'no session';
        pill.title = '/session 不可用，继续无会话请求';
      }
      return Promise.all([loadHealth(), loadConfig()]);
    }).then(function () {
      return loadSchema();
    }).then(function (schema) {
      if (storageTabDisabled) return;
      var dbs = (schema && schema.databases) || [];
      if (dbs.length) loadStorageFiles();
    });

    setInterval(loadHealth, 15000);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
