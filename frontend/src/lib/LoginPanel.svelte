<script>
  export let apiBase
  export let title = 'Login'
  export let subtitle = 'Continue with Google to log in.'
  export let submitLabel = 'Continue with Google'
  export let returnTo = '/'

  let status = 'idle'
  let message = ''

  function sanitizeReturnTo(value) {
    if (!value || typeof value !== 'string') {
      return '/'
    }
    if (!value.startsWith('/') || value.startsWith('//')) {
      return '/'
    }
    return value
  }

  function startGoogleLogin() {
    const safeReturnTo = sanitizeReturnTo(returnTo)

    try {
      const url = new URL(`${apiBase}/api/google_oauth2/start`)
      url.searchParams.set('return_to', safeReturnTo)
      status = 'loading'
      message = ''
      window.location.assign(url.toString())
    } catch {
      status = 'error'
      message = 'Failed to start Google login.'
    }
  }
</script>

<div class="login-section">
  <h2>{title}</h2>
  <p class="subtitle">{subtitle}</p>

  <button
    type="button"
    class="btn btn-primary"
    disabled={status === 'loading'}
    on:click={startGoogleLogin}
  >
    {status === 'loading' ? 'Redirecting...' : submitLabel}
  </button>

  {#if message}
    <p class="msg msg-error">{message}</p>
  {/if}
</div>

<style>
  .login-section {
    width: 100%;
  }
</style>
