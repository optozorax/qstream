<script>
  import { onMount } from 'svelte'

  const apiBase = import.meta.env.VITE_API_BASE_URL ?? 'http://localhost:3000'
  const siteKey = import.meta.env.VITE_HCAPTCHA_SITE_KEY ?? ''

  let nickname = ''
  let status = 'idle'
  let message = ''
  let user = null

  let captchaContainer
  let widgetId = null
  let captchaToken = ''
  let captchaReady = false

  const HCAPTCHA_SCRIPT_ID = 'hcaptcha-script'

  function loadHcaptchaScript() {
    return new Promise((resolve, reject) => {
      if (window.hcaptcha) {
        resolve(window.hcaptcha)
        return
      }

      const existingScript = document.getElementById(HCAPTCHA_SCRIPT_ID)
      if (existingScript) {
        existingScript.addEventListener(
          'load',
          () => resolve(window.hcaptcha),
          { once: true }
        )
        existingScript.addEventListener(
          'error',
          () => reject(new Error('Failed to load hCaptcha script')),
          { once: true }
        )
        return
      }

      const script = document.createElement('script')
      script.id = HCAPTCHA_SCRIPT_ID
      script.src = 'https://js.hcaptcha.com/1/api.js?render=explicit'
      script.async = true
      script.defer = true
      script.onload = () => resolve(window.hcaptcha)
      script.onerror = () => reject(new Error('Failed to load hCaptcha script'))
      document.head.append(script)
    })
  }

  async function setupCaptcha() {
    if (!siteKey) {
      status = 'error'
      message = 'Set VITE_HCAPTCHA_SITE_KEY in frontend/.env before running.'
      return
    }

    try {
      const hcaptcha = await loadHcaptchaScript()
      widgetId = hcaptcha.render(captchaContainer, {
        sitekey: siteKey,
        callback: (token) => {
          captchaToken = token
          if (status === 'error') {
            status = 'idle'
            message = ''
          }
        },
        'expired-callback': () => {
          captchaToken = ''
        },
        'error-callback': () => {
          captchaToken = ''
          status = 'error'
          message = 'hCaptcha returned an error. Please retry.'
        }
      })
      captchaReady = true
    } catch {
      status = 'error'
      message = 'Failed to initialize hCaptcha widget.'
    }
  }

  onMount(() => {
    setupCaptcha()

    return () => {
      if (window.hcaptcha && widgetId !== null) {
        window.hcaptcha.remove(widgetId)
      }
    }
  })

  async function submitForm(event) {
    event.preventDefault()

    const trimmedNickname = nickname.trim()
    if (!trimmedNickname) {
      status = 'error'
      message = 'Enter a nickname.'
      return
    }

    if (!captchaToken) {
      status = 'error'
      message = 'Complete hCaptcha first.'
      return
    }

    status = 'loading'
    message = ''
    user = null

    try {
      const response = await fetch(`${apiBase}/api/register`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          nickname: trimmedNickname,
          hcaptcha_token: captchaToken
        })
      })

      const payload = await response.json().catch(() => ({}))
      if (!response.ok) {
        throw new Error(payload.error ?? `Request failed with status ${response.status}`)
      }

      user = payload.user
      status = 'success'
      message = 'Nickname saved successfully.'
      nickname = ''
    } catch (error) {
      status = 'error'
      message = error instanceof Error ? error.message : 'Unexpected error happened.'
    } finally {
      if (window.hcaptcha && widgetId !== null) {
        window.hcaptcha.reset(widgetId)
      }
      captchaToken = ''
    }
  }
</script>

<main>
  <section class="panel">
    <p class="eyebrow">QStream MVP</p>
    <h1>Sign in with hCaptcha</h1>
    <p class="hint">Pass hCaptcha, enter your nickname, and we save it to SQLite through the Rust API.</p>

    <form on:submit={submitForm}>
      <label for="nickname">Nickname</label>
      <input
        id="nickname"
        type="text"
        maxlength="32"
        bind:value={nickname}
        placeholder="your_nick"
        required
      />

      <div class="captcha-slot" bind:this={captchaContainer}></div>

      <button type="submit" disabled={status === 'loading' || !captchaReady}>
        {status === 'loading' ? 'Saving...' : 'Save nickname'}
      </button>
    </form>

    {#if message}
      <p class={`message ${status}`}>{message}</p>
    {/if}

    {#if user}
      <div class="result">
        <strong>User saved:</strong>
        <code>id={user.id}, nickname={user.nickname}</code>
      </div>
    {/if}
  </section>
</main>
