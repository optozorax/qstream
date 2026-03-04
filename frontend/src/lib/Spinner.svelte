<script>
  export let size = 16
  export let color = 'currentColor'
</script>

<svg
  class="spinner"
  width={size}
  height={size}
  viewBox="0 0 16 16"
  fill="none"
  aria-hidden="true"
>
  <circle cx="8" cy="8" r="6" stroke={color} stroke-width="2" stroke-opacity="0.25" />
  <path d="M14 8a6 6 0 0 0-6-6" stroke={color} stroke-width="2" stroke-linecap="round" />
</svg>

<style>
  .spinner {
    animation: spin 0.7s linear infinite;
    display: inline-block;
    vertical-align: middle;
    flex-shrink: 0;
  }

  @keyframes spin {
    to { transform: rotate(360deg); }
  }
</style>
