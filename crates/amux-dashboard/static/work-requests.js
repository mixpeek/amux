/* Native source-backed board detail. Source text is rendered as text, never HTML. */
window.AmuxWorkRequests = (() => {
  let filtered = false, current = null, generation = 0, timer = null, busy = false;
  let lastStatus = 0, statusFlight = null, detailFlight = null;
  const retries = new Map();
  const $ = id => document.getElementById(id);
  const node = (tag, text, cls) => {
    const n = document.createElement(tag);
    if (text !== undefined && text !== null) n.textContent = String(text);
    if (cls) n.className = cls;
    return n;
  };
  const labels = {backlog:'접수',todo:'대기',doing:'진행 중',review:'검토 필요',needsyou:'확인 필요',blocked:'보류',done:'완료',verified:'확인 완료',discarded:'제외',dismissed:'제외',ready_for_review:'검토 필요',approved:'승인됨',delivered:'전달 완료',stale:'원문 변경 · 재작성 필요',pending:'대기',queued:'대기',claimed:'작성 중',accepted:'접수됨',imported:'초안 반영됨',failed:'실패',unknown:'결과 확인 필요',held:'보류',awaiting_draft:'초안 작성 가능',drafting:'초안 작성 중',needs_review:'요청 검토',review_ready:'검토 필요',draft_failed:'초안 작성 실패',waiting_information:'정보 확인 필요',needs_input:'정보 확인 필요',already_resolved:'이미 해결됨',not_owner:'담당 대상 아님',not_actionable:'처리 대상 아님'};
  function label(value) { return labels[value] || value || '접수'; }
  async function read(url, options) {
    const response = await apiCall(API + url, options);
    if (!response) throw new Error('연결 상태를 확인해 주세요.');
    let value;
    try { value = await response.json(); } catch (_) { throw new Error('서버 응답을 읽지 못했습니다.'); }
    if (!response.ok) throw new Error(value.message || value.error || ('요청 실패 (' + response.status + ')'));
    return value;
  }
  function feedback(text, error = false) {
    const box = $('wr-feedback');
    if (box) { box.textContent = text; box.classList.toggle('wr-error', error); }
  }
  function button(text, handler, primary = false) {
    const b = node('button', text, 'btn' + (primary ? ' primary' : ''));
    b.type = 'button'; b.disabled = busy;
    b.addEventListener('click', handler); return b;
  }
  function section(title, content) {
    const n = node('section', null, 'wr-section');
    n.append(node('h3', title), content); return n;
  }
  function safeLink(url, text) {
    try {
      const u = new URL(url);
      if (u.protocol !== 'https:' || !/(^|\.)slack\.com$/.test(u.hostname)) return null;
      const a = node('a', text); a.href = u.href; a.target = '_blank'; a.rel = 'noopener noreferrer'; return a;
    } catch (_) { return null; }
  }
  function confirmButton(text, confirmation, handler) {
    const wrap = node('div', null, 'wr-confirm');
    const check = node('input'); check.type = 'checkbox';
    const line = node('label'); line.append(check, node('span', confirmation));
    const b = button(text, handler, true); b.disabled = true;
    check.addEventListener('change', () => { b.disabled = busy || !check.checked; });
    wrap.append(line, b); return wrap;
  }
  function render(value) {
    const panel = $('wr-detail'); if (!panel) return;
    const detail = value.detail || {}, issue = detail.issue || detail.request || {};
    const artifact = detail.artifact, approval = detail.approval, job = detail.automation || detail.assessment;
    const allowed = detail.allowed_actions || value.allowed_actions;
    const actionsBlocked = ['held','done','dismissed','drafting'].includes(value.status) || ['pending','unknown'].includes(value.operation?.state);
    const can = kind => (Array.isArray(allowed) ? allowed.includes(kind) : true) && (!actionsBlocked || ['hold','restore'].includes(kind));
    current = value;
    panel.replaceChildren();
    const heading = node('div', null, 'wr-heading');
    const headingState = ['held','dismissed','drafting','draft_failed'].includes(value.status) ? value.status : artifact?.status || value.status;
    heading.append(node('h2', '업무 요청'), node('span', label(headingState), 'wr-state'));
    heading.append(button('새로고침', () => refresh(true)));
    panel.append(heading);
    const source = node('div', null, 'wr-source');
    source.append(node('p', issue.requester_name || issue.requester_id || '요청자 정보 없음', 'wr-muted'));
    const sourceText = detail.context?.request?.text || issue.text || issue.request_text || issue.title || value.title;
    source.append(node('pre', sourceText || '원문을 확인할 수 없습니다.'));
    const link = safeLink(issue.evidence_permalink || issue.permalink, 'Slack 원문 열기'); if (link) source.append(link);
    if (issue.channel_name || issue.channel_id) source.append(node('p', issue.channel_name || issue.channel_id, 'wr-muted'));
    panel.append(section('요청 내용', source));
    if (job?.work_summary || job?.rationale || job?.last_error) {
      const assessment = node('div');
      if (job.work_summary) assessment.append(node('p', job.work_summary));
      if (job.rationale) assessment.append(node('p', job.rationale, 'wr-muted'));
      if (job.last_error) assessment.append(node('p', job.last_error, 'wr-error'));
      panel.append(section('처리 상태', assessment));
    }
    const request = detail.draft_request, execution = detail.execution;
    if (request && ['queued','claimed'].includes(request.state)) panel.append(node('p', '초안 ' + label(request.state) + (execution ? ' · ' + label(execution.status) : ''), 'wr-progress'));
    const controls = node('div', null, 'wr-actions');
    if (can('generate')) controls.append(button(artifact ? '초안 다시 작성' : '초안 작성', () => act('generate'), true));
    if (can('hold')) controls.append(button('보류', () => act('hold')));
    if (can('restore')) controls.append(button('다시 진행', () => act('restore')));
    panel.append(controls);
    if (artifact) {
      const content = node('div');
      content.append(node('p', '버전 ' + artifact.version + ' · ' + label(artifact.status), 'wr-muted'));
      content.append(node('pre', artifact.content || '', 'wr-artifact'));
      if (artifact.output_path || artifact.download_available) {
        const a = node('a', '파일 다운로드', 'btn');
        a.href = _authUrl(API + '/api/board/' + encodeURIComponent(value.task_id) + '/source/artifacts/' + encodeURIComponent(artifact.id) + '/download');
        content.append(a);
      }
      if (can('revise')) {
        const revision = node('textarea'); revision.id = 'wr-revision'; revision.placeholder = '어떤 부분을 수정할까요?'; revision.maxLength = 4000; revision.rows = 3;
        revision.setAttribute('aria-label','초안 수정 요청');
        content.append(revision, button('수정 요청', () => {
          const instruction = revision.value.trim();
          if (!instruction) { revision.focus(); feedback('수정할 내용을 입력해 주세요.', true); return; }
          act('revise', {instruction,artifact_id:artifact.id,content_hash:artifact.content_hash});
        }));
      }
      const destination = node('div', null, 'wr-destination');
      destination.append(node('strong', '수신 위치'), node('p', '채널 ' + (artifact.recipient_channel || '없음') + '\n스레드 ' + (artifact.recipient_thread_ts || '없음')));
      content.append(destination);
      if (can('approve') && artifact.status === 'ready_for_review') content.append(confirmButton('이 버전 승인', '초안 전체와 수신 위치를 확인했습니다. 승인은 발송하지 않습니다.', () => act('approve', {
        artifact_id:artifact.id, content_hash:artifact.content_hash,
        recipient_channel:artifact.recipient_channel, recipient_thread_ts:artifact.recipient_thread_ts
      })));
      if (can('deliver') && artifact.status === 'approved' && approval?.state === 'approved') content.append(confirmButton('Slack으로 전송', '승인한 이 버전을 위 수신 위치로 전송합니다.', () => act('deliver', {artifact_id:artifact.id,content_hash:artifact.content_hash})));
      if (artifact.status === 'delivered') content.append(node('p', '전달 완료가 기록되었습니다.','wr-progress'));
      panel.append(section('검토할 초안', content));
    }
    const evidence = node('div');
    for (const item of detail.evidence || []) {
      const entry = node('div', null, 'wr-evidence');
      entry.append(node('p', [item.author_id,item.message_at].filter(Boolean).join(' · '), 'wr-muted'), node('pre', item.text || ''));
      const a = safeLink(item.permalink, '근거 원문'); if (a) entry.append(a); evidence.append(entry);
    }
    if (evidence.children.length) panel.append(section('판단 근거', evidence));
    const attachments = detail.attachments || [];
    if (attachments.length) panel.append(section('첨부 자료', node('p', attachments.map(a => a.title || a.name || a.filename || a.id).join('\n'))));
    const feedbackBox = node('p', '', 'wr-feedback'); feedbackBox.id = 'wr-feedback'; feedbackBox.setAttribute('role','status'); feedbackBox.setAttribute('aria-live','polite'); panel.append(feedbackBox);
    if (value.operation && ['pending','unknown','failed'].includes(value.operation.state)) feedback('요청 상태: ' + label(value.operation.state) + (value.operation.error ? ' · ' + value.operation.error : ''), value.operation.state !== 'pending');
  }
  async function refresh(force = false) {
    if (!current || busy || detailFlight === generation || !document.getElementById('board-detail-overlay')?.classList.contains('active')) return;
    if (!force && $('wr-revision')?.value.trim()) return;
    const id = current.task_id, version = generation; detailFlight = version;
    try {
      const value = await read('/api/board/' + encodeURIComponent(id) + '/source');
      if (version !== generation || current?.task_id !== id) return;
      const next = {...value,task_id:id};
      if (force || JSON.stringify(next) !== JSON.stringify(current)) render(next);
    } catch (e) { if (version === generation) feedback('요청을 갱신하지 못했습니다: ' + e.message, true); }
    finally { if (detailFlight === version) detailFlight = null; }
  }
  async function act(kind, fields = {}) {
    if (!current || busy) return;
    const source = current, id = source.task_id, version = generation;
    const command = {kind,expected_source_fingerprint:source.source_fingerprint,...fields};
    const signature = JSON.stringify(command), retryKey = id + ':' + signature;
    const operation_id = retries.get(retryKey) || crypto.randomUUID(); retries.set(retryKey,operation_id);
    busy = true; document.querySelectorAll('#wr-detail button').forEach(b => { b.disabled = true; }); feedback('요청을 처리하고 있습니다.');
    try {
      const value = await read('/api/board/' + encodeURIComponent(id) + '/source/actions', {method:'POST',headers:{'Content-Type':'application/json','X-Amux-Approver':'dashboard'},body:JSON.stringify({...command,operation_id})});
      const operationState = value.operation?.state;
      if (!['pending','unknown'].includes(operationState)) retries.delete(retryKey);
      if (version !== generation) return;
      busy = false;
      if (value.detail) render({...value,task_id:id}); else await refresh(true);
      if (['failed','unknown'].includes(operationState)) feedback('요청 상태: ' + label(operationState) + (value.operation.error ? ' · ' + value.operation.error : ''), true);
      else feedback(operationState === 'pending' ? '요청이 접수됐습니다. 결과가 준비되면 이 카드에 표시됩니다.' : '요청이 반영됐습니다.');
      fetchBoard();
    } catch (e) { if (version === generation) feedback('반영 여부를 확인하지 못했습니다. 같은 작업을 다시 누르면 동일 요청으로 확인합니다. ' + e.message,true); }
    finally { if (version === generation) { busy = false; document.querySelectorAll('#wr-detail button').forEach(b => { b.disabled = b.parentElement.classList.contains('wr-confirm') ? !b.parentElement.querySelector('input')?.checked : false; }); } }
  }
  function close() { generation++; current = null; busy = false; clearInterval(timer); timer = null; }
  function open(item) {
    close(); const panel = $('wr-detail'), overlay = $('board-detail-overlay');
    const managed = item.source === 'workdesk'; overlay?.classList.toggle('wr-managed',managed);
    const title = $('bd-title'); if (title) title.readOnly = managed;
    if (!panel) return; panel.hidden = !managed; panel.replaceChildren();
    if (!managed) return;
    current = {task_id:item.id}; panel.append(node('p','요청과 초안을 불러오는 중입니다.','wr-muted'));
    refresh(true); timer = setInterval(() => refresh(),5000);
  }
  async function status(force = false) {
    if (statusFlight || (!force && Date.now() - lastStatus < 30000)) return;
    lastStatus = Date.now(); statusFlight = true;
    try {
      const value = await read('/api/work-requests/status');
      const bar = $('wr-toolbar'); if (!bar) return;
      const hasManaged = typeof boardItems !== 'undefined' && boardItems.some(item => item.source === 'workdesk');
      bar.hidden = !hasManaged && (value.configured === false || value.enabled === false);
      if (!bar.hidden) {
        const heading = document.querySelector('.board-page-heading h1'); if (heading) heading.textContent = '업무 보드';
        const subtitle = document.querySelector('.board-page-heading p'); if (subtitle) subtitle.textContent = '요청 접수부터 초안 검토와 승인까지 이곳에서 처리합니다.';
        const tab = document.querySelector('#tab-board .tab-lbl'); if (tab) tab.textContent = '업무';
        const error = value.error || value.last_sync?.error;
        $('wr-connection').textContent = error ? '연결 확인 필요: ' + (typeof error === 'string' ? error : error.error || '요청을 갱신하지 못했습니다.') : '업무 요청 연결됨';
      }
    } catch (e) { const line = $('wr-connection'); if (line) line.textContent = '업무 연결 상태 확인 실패'; }
    finally { statusFlight = null; }
  }
  async function sync() {
    const button = $('wr-sync'); if (button) button.disabled = true;
    try { await read('/api/work-requests/sync',{method:'POST',headers:{'Content-Type':'application/json'},body:'{}'}); await fetchBoard(); await status(true); }
    catch (e) { $('wr-connection').textContent = '동기화 실패: ' + e.message; }
    finally { if (button) button.disabled = false; }
  }
  $('wr-filter')?.addEventListener('click', () => { filtered = !filtered; $('wr-filter').setAttribute('aria-pressed',String(filtered)); renderBoard(); });
  $('wr-sync')?.addEventListener('click',sync);
  status(true);
  return {get filtered() {return filtered;},open,close,status,refresh};
})();
