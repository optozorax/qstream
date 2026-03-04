<script>
  export let history = []
  export let toasts = []

  let showHistory = false

  function formatTime(ts) {
    return new Date(ts).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit', second: '2-digit' })
  }
</script>

<!-- Toast stack -->
<div class="notif-stack">
  {#each toasts as n (n.id)}
    <div class="notif notif-{n.type}">
      <span class="notif-time">{formatTime(n.time)}</span>
      {n.msg}
    </div>
  {/each}
</div>

<!-- History button -->
<button
  type="button"
  class="notif-btn"
  on:click={() => (showHistory = !showHistory)}
  title="Notification history"
>
  <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
    <circle cx="8" cy="8" r="6.5" stroke="currentColor" stroke-width="1.5"/>
    <path d="M8 5v3.5l2 1.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
  </svg>
</button>

<!-- History panel -->
{#if showHistory}
  <!-- svelte-ignore a11y-click-events-have-key-events a11y-no-static-element-interactions -->
  <div class="notif-backdrop" on:click={() => (showHistory = false)}></div>
  <div class="notif-panel">
    <div class="notif-panel-header">
      <span>Notifications</span>
      <button type="button" class="notif-panel-close" on:click={() => (showHistory = false)}>✕</button>
    </div>
    <div class="notif-panel-body">
      {#if history.length === 0}
        <p class="notif-panel-empty">No notifications yet.</p>
      {:else}
        {#each history as n (n.id)}
          <div class="notif-row notif-row-{n.type}">
            <span class="notif-row-msg">{n.msg}</span>
            <span class="notif-row-time">{formatTime(n.time)}</span>
          </div>
        {/each}
      {/if}
    </div>
  </div>
{/if}

<style>
  /* Toast stack */
  .notif-stack {
    position: fixed;
    bottom: 64px;
    right: 20px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    z-index: 500;
    pointer-events: none;
  }

  .notif {
    padding: 9px 14px;
    border-radius: var(--radius-sm);
    font-size: 0.875rem;
    box-shadow: var(--shadow-md);
    display: flex;
    align-items: baseline;
    gap: 8px;
    animation: notif-in 0.2s ease, notif-out 0.3s ease 2.7s forwards;
  }

  .notif-info {
    background: var(--ink);
    color: #fff;
  }

  .notif-error {
    background: var(--red);
    color: #fff;
  }

  .notif-time {
    font-size: 0.75rem;
    opacity: 0.65;
    white-space: nowrap;
    flex-shrink: 0;
  }

  @keyframes notif-in {
    from { opacity: 0; transform: translateY(8px); }
    to   { opacity: 1; transform: translateY(0); }
  }

  @keyframes notif-out {
    to { opacity: 0; }
  }

  /* History button */
  .notif-btn {
    position: fixed;
    bottom: 20px;
    right: 20px;
    z-index: 501;
    width: 36px;
    height: 36px;
    border-radius: 50%;
    border: none;
    background: var(--bg-card);
    color: var(--ink-secondary);
    box-shadow: var(--shadow-md);
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .notif-btn:hover {
    color: var(--ink);
    background: var(--bg-hover);
  }

  /* Backdrop */
  .notif-backdrop {
    position: fixed;
    inset: 0;
    z-index: 502;
  }

  /* History panel */
  .notif-panel {
    position: fixed;
    bottom: 64px;
    right: 20px;
    z-index: 503;
    width: 320px;
    max-height: 400px;
    background: var(--bg-card);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-lg);
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .notif-panel-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
    font-weight: 600;
    font-size: 0.875rem;
  }

  .notif-panel-close {
    background: none;
    border: none;
    cursor: pointer;
    color: var(--ink-secondary);
    font-size: 0.8rem;
    padding: 2px 4px;
    line-height: 1;
  }

  .notif-panel-close:hover {
    color: var(--ink);
  }

  .notif-panel-body {
    overflow-y: auto;
    flex: 1;
  }

  .notif-panel-empty {
    padding: 16px;
    font-size: 0.875rem;
    color: var(--ink-secondary);
    margin: 0;
  }

  .notif-row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    padding: 10px 16px;
    border-bottom: 1px solid var(--border);
    font-size: 0.875rem;
  }

  .notif-row:last-child {
    border-bottom: none;
  }

  .notif-row-error .notif-row-msg {
    color: var(--red);
  }

  .notif-row-msg {
    flex: 1;
    min-width: 0;
  }

  .notif-row-time {
    font-size: 0.75rem;
    color: var(--ink-tertiary);
    white-space: nowrap;
    flex-shrink: 0;
  }
</style>
