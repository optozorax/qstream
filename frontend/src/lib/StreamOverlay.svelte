<script>
  import { onDestroy, onMount } from 'svelte'

  export let code
  export let apiBase

  let sessionData = null
  let questions = []
  let qrSvg = ''
  let refreshTimer = null

  const MAX_QUESTIONS = 7
  const REFRESH_INTERVAL = 30_000

  async function loadData() {
    try {
      const resp = await fetch(`${apiBase}/api/sessions/${encodeURIComponent(code)}/questions?sort=top`)
      if (!resp.ok) return
      const payload = await resp.json()
      sessionData = payload.session
      questions = (payload.questions ?? []).slice(0, MAX_QUESTIONS)
    } catch {
      // silently ignore
    }
  }

  onMount(async () => {
    await loadData()
    refreshTimer = setInterval(loadData, REFRESH_INTERVAL)

    try {
      const pageUrl = `${window.location.origin}/s/${code}`
      const QRCode = (await import('qrcode')).default
      qrSvg = await QRCode.toString(pageUrl, {
        type: 'svg',
        margin: 1,
        width: 180,
        color: { dark: '#e2e8f0', light: '#0f172a' }
      })
    } catch {
      // silently ignore
    }
  })

  onDestroy(() => {
    if (refreshTimer !== null) clearInterval(refreshTimer)
  })

  function scoreColor(score) {
    if (score > 5) return '#4ade80'
    if (score > 0) return '#86efac'
    if (score === 0) return '#94a3b8'
    return '#f87171'
  }
</script>

<div class="overlay-root">
  <div class="overlay-header">
    <h1 class="session-name">{sessionData?.name ?? 'Loading...'}</h1>
    {#if sessionData?.description}
      <p class="session-desc">{sessionData.description}</p>
    {/if}
  </div>

  <div class="overlay-body">
    {#if questions.length === 0}
      <p class="empty-msg">No questions yet.</p>
    {:else}
      {#each questions as q}
        <div class="q-row" class:answering={q.is_answering === 1} class:answered={q.is_answered === 1}>
          <span class="q-score" style="color: {scoreColor(q.score)}">
            {q.score > 0 ? '+' : ''}{q.score}
          </span>
          <span class="q-text">
            {#if q.is_answering === 1}
              <span class="q-badge answering-badge">Answering</span>
            {:else if q.is_answered === 1}
              <span class="q-badge answered-badge">Done</span>
            {/if}
            {q.body}
          </span>
        </div>
      {/each}
    {/if}
  </div>

  {#if qrSvg}
    <div class="qr-corner">
      {@html qrSvg}
      <p class="qr-label">Ask at this page</p>
    </div>
  {/if}
</div>

<style>
  .overlay-root {
    min-height: 100vh;
    background: #0f172a;
    color: #f8fafc;
    font-family: 'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
    padding: 36px 240px 36px 36px;
    box-sizing: border-box;
    position: relative;
  }

  .overlay-header {
    margin-bottom: 28px;
  }

  .session-name {
    font-size: 2rem;
    font-weight: 800;
    margin: 0;
    line-height: 1.2;
    color: #f8fafc;
    letter-spacing: -0.02em;
  }

  .session-desc {
    margin: 6px 0 0;
    font-size: 1rem;
    color: #94a3b8;
  }

  .overlay-body {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .q-row {
    display: flex;
    align-items: flex-start;
    gap: 16px;
    background: rgba(255, 255, 255, 0.05);
    border-radius: 10px;
    padding: 14px 18px;
    border-left: 4px solid transparent;
  }

  .q-row.answering {
    border-left-color: #facc15;
    background: rgba(250, 204, 21, 0.08);
  }

  .q-row.answered {
    opacity: 0.45;
  }

  .q-score {
    font-size: 1.3rem;
    font-weight: 700;
    min-width: 52px;
    text-align: right;
    flex-shrink: 0;
    line-height: 1.5;
    font-variant-numeric: tabular-nums;
  }

  .q-text {
    font-size: 1.05rem;
    line-height: 1.55;
    color: #e2e8f0;
  }

  .q-badge {
    display: inline-block;
    font-size: 0.7rem;
    font-weight: 700;
    border-radius: 4px;
    padding: 2px 8px;
    margin-right: 8px;
    vertical-align: middle;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .answering-badge {
    background: #facc15;
    color: #0f172a;
  }

  .answered-badge {
    background: #4ade80;
    color: #0f172a;
  }

  .empty-msg {
    color: #475569;
    font-size: 1.1rem;
  }

  .qr-corner {
    position: fixed;
    bottom: 28px;
    right: 28px;
    display: flex;
    flex-direction: column;
    align-items: center;
    background: rgba(255, 255, 255, 0.04);
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 12px;
    padding: 12px 12px 8px;
  }

  .qr-corner :global(svg) {
    width: 160px;
    height: 160px;
    border-radius: 6px;
    display: block;
  }

  .qr-label {
    margin: 8px 0 0;
    font-size: 0.75rem;
    color: #64748b;
    text-align: center;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
</style>
