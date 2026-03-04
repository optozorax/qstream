<script>
  import { onDestroy, createEventDispatcher } from 'svelte'

  let className = ''
  export { className as class }
  export let label
  export let confirmLabel
  export let disabled = false

  const dispatch = createEventDispatcher()
  const DURATION = 5000

  let confirming = false
  let timer = null

  function handleClick() {
    if (confirming) {
      cancel()
      dispatch('confirm')
    } else {
      confirming = true
      timer = setTimeout(cancel, DURATION)
    }
  }

  function cancel() {
    confirming = false
    if (timer !== null) {
      clearTimeout(timer)
      timer = null
    }
  }

  onDestroy(cancel)
</script>

<button type="button" class={className} on:click={handleClick} {disabled}>
  {#if confirming}
    <svg class="countdown" width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <circle class="track" cx="6" cy="6" r="4.5" />
      <circle cx="6" cy="6" r="4.5" />
    </svg>
  {/if}{confirming ? confirmLabel : label}
</button>

<style>
  .countdown {
    display: inline-block;
    margin-right: 5px;
    vertical-align: -2px;
    flex-shrink: 0;
  }

  .countdown circle {
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
  }

  .countdown .track {
    opacity: 0.25;
  }

  .countdown circle:not(.track) {
    stroke-dasharray: 28.27;
    stroke-dashoffset: 0;
    transform: scale(-1, 1) rotate(-90deg);
    transform-origin: 6px 6px;
    animation: drain 5s linear forwards;
  }

  @keyframes drain {
    from { stroke-dashoffset: 0; }
    to   { stroke-dashoffset: 28.27; }
  }
</style>
