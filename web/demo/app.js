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
    storage: { db: null, file: null, unit: null, files: [], overview: null }
  };

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
    }).catch(function () {
      /* keep defaults; the storage panel surfaces its own 404 notice if disabled */
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
      state.storage.db = null;
      return;
    }
    var names = dbs.map(function (d) { return d.name; });
    var current = state.storage.db;
    if (!current || names.indexOf(current) < 0) current = dbs[0].name;
    dbs.forEach(function (db) {
      sel.append(el('option', {
        text: db.name + (db.system ? ' · 系统' : ''),
        attrs: { value: db.name }
      }));
    });
    state.storage.db = current;
    sel.value = current;
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
  /* Occupancy-grid colours, keyed by the `units[].kind` values from /api/overview. */
  var UNIT_COLORS = {
    file_header: '#8b949e',
    slotted: '#58a6ff',
    pax: '#7ee787',
    leaf: '#3fb950',
    internal: '#2ea043',
    insert: '#a5d6ff',
    delete: '#ffa657',
    commit: '#d2a8ff',
    record: '#ffa657',
    data: '#7ee787',
    bloom: '#e3b341',
    index: '#79c0ff',
    footer: '#bc8cff',
    catalog: '#d2a8ff',
    manifest: '#ffd88a',
    header: '#8b949e',
    empty: '#39414d',
    unknown: '#5b6472'
  };

  /* Hex-annotation colours, keyed by `fields[].kind` from /api/page. */
  var FIELD_COLORS = {
    magic: '#d2a8ff',
    version: '#a5d6ff',
    kind: '#79c0ff',
    node_type: '#79c0ff',
    size: '#56d4dd',
    count: '#ffa657',
    id: '#ffd88a',
    directory: '#7ee787',
    slot: '#56d364',
    record: '#a5e075',
    free: '#5b6472',
    reserved: '#6e7681',
    page: '#f0883e',
    page_no: '#f0883e',
    highkey: '#ff9e64',
    entry: '#c9a2ff',
    length: '#ff7b72',
    type: '#ff9492',
    trx: '#f778ba',
    payload: '#8ddb8c',
    path: '#9ecbff',
    data: '#7ee787',
    bloom: '#e3b341',
    index: '#58a6ff',
    footer: '#bc8cff',
    body: '#8b949e',
    column: '#7ec8e3'
  };

  var FIELD_FALLBACK = '#7d8794';
  var MODE_LABELS = {
    'heap-row': '行存堆页',
    'heap-pax': 'PAX 列存堆页',
    btree: 'B+ 树节点',
    catalog: '数据库目录',
    wal: '预写日志帧',
    dwb: '双写缓冲记录',
    'lsm-sstable': 'LSM 有序表',
    'lsm-manifest': 'LSM 清单',
    'file-header': '公共文件头',
    unknown: '未知结构'
  };

  /* Safety cap for the hex view so a large SSTable cannot freeze the page. */
  var MAX_HEX_BYTES = 65536;

  function modeLabel(mode) {
    var m = mode == null ? 'unknown' : String(mode);
    return MODE_LABELS[m] ? MODE_LABELS[m] + ' (' + m + ')' : m;
  }

  function hexToRgba(hex, alpha) {
    var m = /^#?([0-9a-f]{6})$/i.exec(String(hex));
    if (!m) return String(hex);
    var n = parseInt(m[1], 16);
    return 'rgba(' + ((n >> 16) & 255) + ',' + ((n >> 8) & 255) + ',' + (n & 255) + ',' + alpha + ')';
  }

  function kindClass(kind) {
    switch (kind) {
      case 'catalog': return 'badge-catalog';
      case 'wal': return 'badge-wal';
      case 'dwb': return 'badge-dwb';
      case 'heap': return 'badge-heap';
      case 'index': return 'badge-index';
      case 'lob': return 'badge-lob';
      case 'lsm_manifest':
      case 'lsm_sstable': return 'badge-lsm';
      default: return 'badge-k';
    }
  }

  function showStorageDisabled() {
    var notice = $('#storage-disabled');
    if (notice) {
      notice.hidden = false;
      notice.textContent = '存储预览已关闭（在 config.toml 设 [web] page_preview = true）';
    }
    var viewer = $('#storage-viewer');
    if (viewer) viewer.hidden = true;
  }

  function fileUnitText(f) {
    var n = f && f.units != null ? f.units : '?';
    var uk = f && f.unit_kind && f.unit_kind !== 'none' ? String(f.unit_kind) : 'unit';
    return n + ' ' + uk + (n === 1 ? '' : 's');
  }

  function renderFileList(files) {
    var list = $('#storage-files');
    clear(list);
    if (!files.length) {
      list.append(el('span', { className: 'muted', text: '（无文件）' }));
      return;
    }
    var frag = document.createDocumentFragment();
    files.forEach(function (f) {
      var kind = f.kind == null ? 'other' : String(f.kind);
      var meta = el('span', { className: 'f-meta' });
      meta.append(badge(kind, kindClass(kind)));
      meta.append(el('span', { className: 'f-size', text: fmtBytes(f.size) }));
      if (f.engine || f.layout) {
        meta.append(el('span', { text: [f.engine, f.layout].filter(Boolean).join('/') }));
      }
      if (f.table) meta.append(el('span', { className: 'f-tag', text: 'table ' + f.table }));
      if (f.index) meta.append(el('span', { className: 'f-tag', text: 'index ' + f.index }));
      meta.append(el('span', { text: fileUnitText(f) }));
      frag.append(el('button', {
        className: 'file-item',
        dataset: { file: f.name },
        title: f.name,
        attrs: { type: 'button' },
        on: { click: function () { selectFile(f.name, null); } }
      }, [
        el('span', { className: 'f-name', text: f.name }),
        meta
      ]));
    });
    list.append(frag);
  }

  function markActiveFile(name) {
    Array.prototype.forEach.call(document.querySelectorAll('.file-item'), function (b) {
      b.classList.toggle('is-active', b.dataset.file === name);
    });
  }

  function markOccupancyUnit(index) {
    var want = index == null ? null : String(index);
    Array.prototype.forEach.call(document.querySelectorAll('.occ-cell'), function (c) {
      c.classList.toggle('is-active', want != null && c.dataset.unit === want);
    });
  }

  function renderOverview(d) {
    var grid = $('#storage-overview');
    clear(grid);
    var units = d && Array.isArray(d.units) ? d.units : [];
    var title = $('#storage-file-title');
    if (title) title.textContent = d && d.file ? String(d.file) : '';
    var meta = $('#storage-overview-meta');
    if (meta) {
      meta.textContent = [
        modeLabel(d && d.mode),
        d && d.unit_kind ? String(d.unit_kind) : '',
        '× ' + (d && d.total != null ? d.total : 0)
      ].filter(Boolean).join(' · ');
    }
    if (!units.length) {
      grid.append(el('span', { className: 'muted', text: '（无单元）' }));
      return;
    }
    var frag = document.createDocumentFragment();
    units.forEach(function (u) {
      var kind = u && u.kind != null ? String(u.kind) : 'unknown';
      var used = !u || u.used !== false;
      var label = u && u.label != null ? String(u.label) : String(u && u.index != null ? u.index : '');
      var color = UNIT_COLORS[kind] || UNIT_COLORS.unknown;
      var cell = el('button', {
        className: 'occ-cell' + (used ? '' : ' is-unused'),
        text: label,
        title: (u && u.summary != null ? String(u.summary) : label) + ' · ' + kind,
        attrs: { type: 'button' },
        dataset: { unit: u && u.index != null ? u.index : '' },
        on: { click: function () { if (u && u.index != null) loadUnit(u.index); } }
      });
      cell.style.backgroundColor = used ? hexToRgba(color, 0.28) : 'rgba(255,255,255,0.03)';
      cell.style.borderColor = used ? hexToRgba(color, 0.75) : 'var(--border-soft)';
      cell.style.color = used ? color : 'var(--text-faint)';
      frag.append(cell);
    });
    grid.append(frag);
  }

  function parseHexBytes(hex) {
    var out = [];
    var lines = String(hex == null ? '' : hex).split('\n');
    for (var i = 0; i < lines.length; i++) {
      var line = lines[i].trim();
      if (line === '') continue;
      var toks = line.split(/\s+/);
      for (var j = 0; j < toks.length; j++) {
        var v = parseInt(toks[j], 16);
        out.push(isNaN(v) ? 0 : (v & 0xff));
      }
    }
    return out;
  }

  /* byte -> index of the field that covers it. On overlap the smallest
   * (most specific) field wins, which makes "slot i" / "entry i" beat the
   * directory / entry-region spans they sit inside. */
  function buildFieldLookup(fields, n) {
    var byByte = new Int32Array(n);
    for (var i = 0; i < n; i++) byByte[i] = -1;
    for (var fi = 0; fi < fields.length; fi++) {
      var f = fields[fi];
      if (!f) continue;
      var start = f.start | 0;
      var len = f.len | 0;
      if (start < 0 || len <= 0) continue;
      var end = Math.min(start + len, n);
      for (var b = start; b < end; b++) {
        var cur = byByte[b];
        if (cur < 0 || (fields[cur] && fields[cur].len > len)) byByte[b] = fi;
      }
    }
    return byByte;
  }

  function isPrintable(b) { return b >= 0x20 && b <= 0x7e; }
  function hex2(b) { return (b < 16 ? '0' : '') + b.toString(16); }
  function asciiChar(b) { return isPrintable(b) ? String.fromCharCode(b) : '.'; }

  function renderAnnotatedHex(bytes, fields) {
    var shown = Math.min(bytes.length, MAX_HEX_BYTES);
    var wrap = el('div', { className: 'hexdump field-hex' });
    if (shown === 0) {
      wrap.append(el('span', { className: 'muted', text: '（无数据）' }));
      return wrap;
    }
    var byByte = buildFieldLookup(fields, shown);
    var frag = document.createDocumentFragment();
    for (var row = 0; row * 16 < shown; row++) {
      var line = el('div', { className: 'hex-line' });
      line.append(el('span', { className: 'hex-off', text: (row * 16).toString(16).padStart(8, '0') }));
      var hexBox = el('span', { className: 'hex-bytes' });
      var asciiText = '';
      var col = 0;
      while (col < 16) {
        var idx = row * 16 + col;
        if (idx >= shown) {
          var pad = 16 - col;
          hexBox.append(el('span', { className: 'hex-byte is-empty', text: '   '.repeat(pad) }));
          asciiText += ' '.repeat(pad);
          break;
        }
        var fi = byByte[idx];
        var run = 1;
        while (col + run < 16 && row * 16 + col + run < shown && byByte[idx + run] === fi) run++;
        var hexText = '';
        for (var k = 0; k < run; k++) {
          var b = bytes[idx + k];
          hexText += hex2(b) + ' ';
          asciiText += asciiChar(b);
        }
        var span = el('span', {
          className: 'hex-run' + (fi < 0 ? ' is-plain' : ''),
          text: hexText,
          dataset: { field: fi < 0 ? '' : String(fi) }
        });
        var color = fi < 0 ? FIELD_FALLBACK : (FIELD_COLORS[fields[fi].kind] || FIELD_FALLBACK);
        span.style.color = color;
        if (fi >= 0) {
          span.style.backgroundColor = hexToRgba(color, 0.16);
          span.title = fields[fi].label + ' · ' + fields[fi].note;
        }
        hexBox.append(span);
        col += run;
      }
      line.append(hexBox);
      line.append(el('span', { className: 'hex-sep', text: '|' }));
      line.append(el('span', { className: 'hex-ascii', text: asciiText }));
      line.append(el('span', { className: 'hex-sep', text: '|' }));
      frag.append(line);
    }
    wrap.append(frag);
    if (bytes.length > shown) {
      wrap.append(el('div', {
        className: 'muted',
        text: '仅显示前 ' + shown + ' 字节（共 ' + bytes.length + ' 字节）'
      }));
    }
    return wrap;
  }

  function renderLegend(box, fields, hexWrap) {
    clear(box);
    if (!fields || !fields.length) {
      box.append(el('span', { className: 'muted', text: '（无字段注解）' }));
      return;
    }
    var list = el('div', { className: 'legend-list' });
    fields.forEach(function (f, i) {
      var color = FIELD_COLORS[f.kind] || FIELD_FALLBACK;
      var item = el('button', {
        className: 'legend-item',
        attrs: { type: 'button' },
        title: f.label + ' · ' + f.note,
        on: {
          click: function () {
            Array.prototype.forEach.call(hexWrap.querySelectorAll('.hex-run.is-hl'), function (n) {
              n.classList.remove('is-hl');
            });
            var hits = hexWrap.querySelectorAll('.hex-run[data-field="' + i + '"]');
            Array.prototype.forEach.call(hits, function (n) { n.classList.add('is-hl'); });
            if (hits.length) hits[0].scrollIntoView({ block: 'nearest' });
          }
        }
      }, [
        el('span', { className: 'legend-swatch', attrs: { style: 'background:' + color } }),
        el('span', { className: 'legend-text' }, [
          el('span', { className: 'legend-label', text: f.label }),
          el('span', { className: 'legend-note', text: f.note })
        ]),
        el('span', { className: 'legend-range', text: f.start + '..' + (f.start + f.len) })
      ]);
      item.style.borderLeftColor = color;
      list.append(item);
    });
    box.append(list);
  }

  function renderJsonBlock(box, value) {
    if (!box) return;
    clear(box);
    if (value == null) {
      box.append(el('span', { className: 'muted', text: '—' }));
      return;
    }
    var v = value;
    if (typeof v === 'string') {
      try { v = JSON.parse(v); } catch (e) { v = value; }
    }
    var text;
    try { text = JSON.stringify(v, null, 2); } catch (e) { text = String(v); }
    box.append(pre(text));
  }

  function clearUnit() {
    markOccupancyUnit(null);
    var mode = $('#unit-mode');
    if (mode) mode.textContent = '—';
    var index = $('#unit-index');
    if (index) index.textContent = '';
    var desc = $('#unit-description');
    if (desc) desc.textContent = '';
    clear($('#unit-hex'));
    clear($('#unit-legend'));
    clear($('#unit-structure'));
    clear($('#unit-header'));
    var notes = $('#unit-notes');
    if (notes) notes.textContent = '';
    var dot = $('#unit-bytes');
    if (dot) dot.textContent = '';
  }

  function renderUnit(d) {
    d = d || {};
    var no = d.no != null ? d.no : state.storage.unit;
    state.storage.unit = no;
    markOccupancyUnit(no);

    var mode = $('#unit-mode');
    if (mode) mode.textContent = modeLabel(d.mode);
    var index = $('#unit-index');
    if (index) {
      index.textContent = [
        'unit ' + (no == null ? '?' : no),
        'of ' + (d.total_pages == null ? '?' : d.total_pages),
        d.kind ? String(d.kind) : '',
        d.unit_kind ? String(d.unit_kind) : '',
        d.page_size ? d.page_size + ' B/unit' : ''
      ].filter(Boolean).join(' · ');
    }
    var desc = $('#unit-description');
    if (desc) desc.textContent = d.description == null ? '' : String(d.description);

    var fields = Array.isArray(d.fields) ? d.fields : [];
    var hexBox = $('#unit-hex');
    clear(hexBox);
    var bytes = parseHexBytes(d.hex);
    var dot = $('#unit-bytes');
    if (dot) dot.textContent = bytes.length + ' bytes' + (fields.length ? ' · ' + fields.length + ' fields' : '');
    hexBox.append(renderAnnotatedHex(bytes, fields));
    renderLegend($('#unit-legend'), fields, hexBox);

    renderJsonBlock($('#unit-structure'), d.structure);
    renderJsonBlock($('#unit-header'), d.header);

    var notes = $('#unit-notes');
    if (notes) notes.textContent = d.notes == null ? '' : String(d.notes);
  }

  function loadUnit(no) {
    var file = state.storage.file;
    if (!file) return Promise.resolve();
    state.storage.unit = no;
    markOccupancyUnit(no);
    var hexBox = $('#unit-hex');
    clear(hexBox);
    hexBox.append(el('div', { className: 'muted', text: '加载中…' }));
    var url = '/api/page?db=' + encodeURIComponent(state.storage.db) +
      '&file=' + encodeURIComponent(file) +
      '&no=' + encodeURIComponent(no);
    return request('GET', url).then(function (d) {
      renderUnit(d || {});
    }).catch(function (e) {
      if (e.status === 404) { showStorageDisabled(); return; }
      clear(hexBox);
      hexBox.append(el('div', { className: 'banner banner-error', text: e.message }));
    });
  }

  function loadOverview(preferUnit) {
    var grid = $('#storage-overview');
    clear(grid);
    grid.append(el('span', { className: 'muted', text: '加载中…' }));
    var file = state.storage.file;
    if (!file) return Promise.resolve();
    var url = '/api/overview?db=' + encodeURIComponent(state.storage.db) +
      '&file=' + encodeURIComponent(file);
    return request('GET', url).then(function (d) {
      state.storage.overview = d || {};
      renderOverview(d || {});
      var total = d && d.total != null ? d.total : 0;
      var sel = null;
      if (preferUnit != null && preferUnit >= 0 && preferUnit < total) sel = preferUnit;
      else if (state.storage.unit != null && state.storage.unit >= 0 && state.storage.unit < total) sel = state.storage.unit;
      else if (total > 0) sel = 0;
      if (sel == null) { clearUnit(); return; }
      return loadUnit(sel);
    }).catch(function (e) {
      if (e.status === 404) { showStorageDisabled(); return; }
      clear(grid);
      grid.append(el('div', { className: 'banner banner-error', text: e.message }));
    });
  }

  function selectFile(name, unit) {
    state.storage.file = name;
    state.storage.unit = unit == null ? null : unit;
    markActiveFile(name);
    return loadOverview(unit == null ? null : unit);
  }

  function loadStorageFiles(preferredFile, preferredUnit) {
    var list = $('#storage-files');
    var db = state.storage.db;
    if (!db) {
      clear(list);
      list.append(el('span', { className: 'muted', text: '（无数据库）' }));
      return Promise.resolve();
    }
    clear(list);
    list.append(el('span', { className: 'muted', text: '加载中…' }));
    return request('GET', '/api/files?db=' + encodeURIComponent(db)).then(function (d) {
      state.storage.files = d && Array.isArray(d.files) ? d.files : [];
      $('#storage-disabled').hidden = true;
      $('#storage-viewer').hidden = false;
      renderFileList(state.storage.files);
      var count = $('#storage-file-count');
      if (count) count.textContent = state.storage.files.length + (state.storage.files.length === 1 ? ' file' : ' files');
      var files = state.storage.files;
      if (!files.length) {
        state.storage.file = null;
        clear($('#storage-overview'));
        clearUnit();
        return null;
      }
      var names = files.map(function (f) { return f.name; });
      var target;
      if (preferredFile && names.indexOf(preferredFile) >= 0) target = preferredFile;
      else if (state.storage.file && names.indexOf(state.storage.file) >= 0) target = state.storage.file;
      else target = files[0].name;
      return selectFile(target, preferredUnit == null ? null : preferredUnit);
    }).catch(function (e) {
      if (e.status === 404) { showStorageDisabled(); return; }
      clear(list);
      list.append(el('div', { className: 'banner banner-error', text: e.message }));
    });
  }

  function refreshStorage() {
    var btn = $('#storage-refresh');
    if (btn) { btn.disabled = true; btn.textContent = '刷新中…'; }
    var keepDb = state.storage.db;
    var keepFile = state.storage.file;
    var keepUnit = state.storage.unit;
    return loadSchema().then(function () {
      var sel = $('#storage-db');
      var names = databases().map(function (x) { return x.name; });
      if (keepDb && names.indexOf(keepDb) >= 0) {
        state.storage.db = keepDb;
        if (sel) sel.value = keepDb;
      } else if (sel) {
        state.storage.db = sel.value || (names.length ? names[0] : null);
      }
      return loadStorageFiles(keepFile, keepUnit);
    }).catch(function () {
      /* loadSchema / loadStorageFiles render their own errors */
    }).then(function () {
      if (btn) { btn.disabled = false; btn.textContent = '刷新'; }
    });
  }

  function initStoragePanel() {
    var sel = $('#storage-db');
    if (sel) {
      sel.addEventListener('change', function (e) {
        state.storage.db = e.target.value;
        state.storage.file = null;
        state.storage.unit = null;
        loadStorageFiles(null, null);
      });
    }
    var btn = $('#storage-refresh');
    if (btn) btn.addEventListener('click', refreshStorage);
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
    }).then(function () {
      if (databases().length) loadStorageFiles();
    });

    setInterval(loadHealth, 15000);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
