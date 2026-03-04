<script>
  import { onDestroy, onMount } from 'svelte'
  import LoginPanel from './lib/LoginPanel.svelte'
  import ConfirmButton from './lib/ConfirmButton.svelte'

  const LOCAL_HOSTNAMES = new Set(['localhost', '127.0.0.1', '0.0.0.0'])

  function defaultApiBase() {
    if (LOCAL_HOSTNAMES.has(window.location.hostname)) {
      return `${window.location.protocol}//${window.location.hostname}:3000`
    }
    return window.location.origin
  }

  function resolveApiBase(rawApiBase) {
    const fallback = defaultApiBase()
    if (!rawApiBase) {
      return fallback
    }

    try {
      const url = new URL(rawApiBase, window.location.origin)
      const pageIsLocal = LOCAL_HOSTNAMES.has(window.location.hostname)
      const apiIsLocal = LOCAL_HOSTNAMES.has(url.hostname)
      const sameHost = url.hostname === window.location.hostname

      if (!pageIsLocal) {
        if (apiIsLocal || (sameHost && url.port === '3000')) {
          url.protocol = window.location.protocol
          url.hostname = window.location.hostname
          url.port = ''
        } else if (sameHost && window.location.protocol === 'https:' && url.protocol === 'http:') {
          url.protocol = 'https:'
        }
      }

      return url.toString().replace(/\/$/, '')
    } catch {
      return rawApiBase.replace(/\/$/, '')
    }
  }

  const apiBase = resolveApiBase(import.meta.env.VITE_API_BASE_URL)

  const AUTH_TOKEN_KEY = 'qstream_auth_token'
  const USER_KEY = 'qstream_user'
  const INTERACTED_QUESTIONS_PREFIX = 'qstream_interacted_questions'

  let route = parseRoute(window.location.pathname)
  let authToken = localStorage.getItem(AUTH_TOKEN_KEY) ?? ''
  let currentUser = parseStoredUser(localStorage.getItem(USER_KEY))

  let homeMessage = ''

  // User's own sessions (home page list)
  let userSessions = []
  let loadingSessions = false

  // Create session form
  let showCreateForm = false
  let createName = ''
  let createDescription = ''
  let createStreamLink = ''
  let createBusy = false
  let createStatus = ''

  let showSessionLogin = false

  // Session settings panel (admin)
  let showSessionSettings = false
  let settingsName = ''
  let settingsDescription = ''
  let settingsStreamLink = ''
  let settingsBusy = false
  let settingsStatus = ''
  let stoppingSession = false

  // Home page: deleting a session
  let deletingSessionCode = null

  let sessionSort = 'top'
  let sessionData = null
  let questions = []
  let loadingQuestions = false
  let sessionError = ''

  let questionText = ''
  let questionStatus = ''
  let questionBusy = false
  let voteBusy = new Set()
  let moderateBusy = new Set()
  let localVotes = {}
  let interactedQuestionIds = new Set()
  let hideInteracted = false

  let eventSource = null
  let autoRefreshDebounceTimer = null
  let activeSessionCode = null
  let updateMode = 'manual'
  let updateModeTouched = false
  let pendingNewQuestions = 0
  let sseConnected = false

  const AUTO_REFRESH_DEBOUNCE_MS = 300

  $: visibleQuestions =
    hideInteracted && currentUser
      ? questions.filter((question) => !interactedQuestionIds.has(question.id))
      : questions

  onMount(() => {
    void processOauthCallbackAndValidate()

    const onPopState = () => {
      route = parseRoute(window.location.pathname)
    }

    window.addEventListener('popstate', onPopState)

    return () => {
      window.removeEventListener('popstate', onPopState)
    }
  })

  onDestroy(() => {
    disconnectSessionEvents()
    clearAutoRefreshDebounce()
  })

  $: if (route.name === 'session') {
    if (activeSessionCode !== route.code) {
      startSessionView(route.code)
    }
  } else if (activeSessionCode !== null) {
    activeSessionCode = null
    disconnectSessionEvents()
    clearAutoRefreshDebounce()
    sessionData = null
    questions = []
    sessionError = ''
    questionText = ''
    questionStatus = ''
    updateMode = 'manual'
    updateModeTouched = false
    pendingNewQuestions = 0
    hideInteracted = false
    interactedQuestionIds = new Set()
    showSessionSettings = false
    settingsStatus = ''
    stoppingSession = false
  }

  $: if (route.name === 'session' && sessionData) {
    applyDefaultUpdateMode(
      !!currentUser && currentUser.id === sessionData.owner_user_id,
      !!currentUser
    )
  }

  function parseRoute(pathname) {
    const match = pathname.match(/^\/s\/([A-Za-z0-9_-]+)$/)
    if (match) {
      return { name: 'session', code: match[1] }
    }
    return { name: 'home' }
  }

  function parseStoredUser(raw) {
    if (!raw) {
      return null
    }

    try {
      return JSON.parse(raw)
    } catch {
      return null
    }
  }

  function interactedQuestionsStorageKey(sessionCode = activeSessionCode, user = currentUser) {
    if (!sessionCode || !user?.id) {
      return null
    }
    return `${INTERACTED_QUESTIONS_PREFIX}:${user.id}:${sessionCode}`
  }

  function persistInteractedQuestions(sessionCode = activeSessionCode) {
    const key = interactedQuestionsStorageKey(sessionCode)
    if (!key) {
      return
    }
    localStorage.setItem(key, JSON.stringify(Array.from(interactedQuestionIds)))
  }

  function addInteractedQuestion(questionId, sessionCode = activeSessionCode) {
    if (!Number.isInteger(questionId)) {
      return
    }
    if (interactedQuestionIds.has(questionId)) {
      return
    }

    const next = new Set(interactedQuestionIds)
    next.add(questionId)
    interactedQuestionIds = next
    persistInteractedQuestions(sessionCode)
  }

  function rememberCurrentUserAuthoredQuestions(sessionCode = activeSessionCode) {
    if (!currentUser?.id || !questions.length) {
      return
    }

    let changed = false
    const next = new Set(interactedQuestionIds)
    for (const question of questions) {
      if (question.author_user_id === currentUser.id && !next.has(question.id)) {
        next.add(question.id)
        changed = true
      }
    }

    if (changed) {
      interactedQuestionIds = next
      persistInteractedQuestions(sessionCode)
    }
  }

  function loadInteractedQuestions(sessionCode = activeSessionCode, includeAuthored = true) {
    const key = interactedQuestionsStorageKey(sessionCode)
    if (!key) {
      interactedQuestionIds = new Set()
      return
    }

    const raw = localStorage.getItem(key)
    if (!raw) {
      interactedQuestionIds = new Set()
      if (includeAuthored) {
        rememberCurrentUserAuthoredQuestions(sessionCode)
      }
      return
    }

    try {
      const parsed = JSON.parse(raw)
      const ids = Array.isArray(parsed)
        ? parsed.filter((id) => Number.isInteger(id))
        : []
      interactedQuestionIds = new Set(ids)
      if (includeAuthored) {
        rememberCurrentUserAuthoredQuestions(sessionCode)
      }
    } catch {
      interactedQuestionIds = new Set()
      if (includeAuthored) {
        rememberCurrentUserAuthoredQuestions(sessionCode)
      }
    }
  }

  function logout() {
    authToken = ''
    currentUser = null
    userSessions = []
    localVotes = {}
    interactedQuestionIds = new Set()
    hideInteracted = false
    localStorage.removeItem(AUTH_TOKEN_KEY)
    localStorage.removeItem(USER_KEY)
  }

  function isUnauthorizedApiError(error) {
    return !!error && typeof error === 'object' && error.status === 401
  }

  function handleAuthInvalid() {
    if (!authToken && !currentUser) {
      return
    }

    logout()

    if (route.name === 'session') {
      showSessionLogin = true
    }
  }

  function goto(path) {
    if (window.location.pathname === path) {
      return
    }
    history.pushState({}, '', path)
    route = parseRoute(path)
  }

  async function apiRequest(path, options = {}) {
    const headers = new Headers(options.headers ?? {})

    if (!headers.has('Content-Type') && options.body) {
      headers.set('Content-Type', 'application/json')
    }

    if (options.auth && authToken) {
      headers.set('Authorization', `Bearer ${authToken}`)
    }

    const response = await fetch(`${apiBase}${path}`, {
      ...options,
      headers
    })

    const payload = await response.json().catch(() => ({}))
    if (!response.ok) {
      if (options.auth && response.status === 401) {
        handleAuthInvalid()
      }

      const error = new Error(payload.error ?? `Request failed with status ${response.status}`)
      error.status = response.status
      throw error
    }

    return payload
  }

  function parseHashParams(hash) {
    const raw = hash?.startsWith('#') ? hash.slice(1) : hash
    if (!raw) {
      return new URLSearchParams()
    }
    return new URLSearchParams(raw)
  }

  function clearHashFragment() {
    if (!window.location.hash) {
      return
    }
    const cleanUrl = `${window.location.pathname}${window.location.search}`
    history.replaceState({}, '', cleanUrl)
  }

  function oauthErrorMessage(code) {
    switch (code) {
      case 'oauth_denied':
      case 'oauth_provider_error':
        return 'Google login was canceled or denied.'
      case 'missing_state':
      case 'invalid_state':
        return 'Google login failed: invalid auth state. Please retry.'
      case 'missing_authorization_code':
        return 'Google login failed: no authorization code.'
      case 'oauth_token_exchange_failed':
        return 'Google login failed while exchanging token.'
      case 'oauth_userinfo_failed':
        return 'Google login failed while loading profile.'
      default:
        return 'Google login failed. Please retry.'
    }
  }

  async function fetchUserSessions() {
    if (!authToken) return
    loadingSessions = true
    try {
      const payload = await apiRequest('/api/sessions', { auth: true })
      userSessions = payload.sessions ?? []
    } catch {
      // silently ignore — sessions will just be empty
    } finally {
      loadingSessions = false
    }
  }

  async function processOauthCallbackAndValidate() {
    const hashParams = parseHashParams(window.location.hash)
    const tokenFromHash = hashParams.get('auth_token')
    const authError = hashParams.get('auth_error')

    if (tokenFromHash) {
      authToken = tokenFromHash
      localStorage.setItem(AUTH_TOKEN_KEY, authToken)

      try {
        const user = await apiRequest('/api/me', { auth: true })
        if (user && typeof user === 'object') {
          currentUser = user
          localStorage.setItem(USER_KEY, JSON.stringify(currentUser))
          if (route.name === 'session') {
            showSessionLogin = false
            questionStatus = 'Logged in. You can ask and vote now.'
            localVotes = {}
            loadInteractedQuestions(route.code)
          } else {
            homeMessage = 'Logged in successfully.'
            void fetchUserSessions()
          }
        }
      } catch {
        logout()
        if (route.name === 'session') {
          questionStatus = 'Google login failed. Please retry.'
        } else {
          homeMessage = 'Google login failed. Please retry.'
        }
      } finally {
        clearHashFragment()
      }
    } else if (authError) {
      const message = oauthErrorMessage(authError)
      if (route.name === 'session') {
        questionStatus = message
      } else {
        homeMessage = message
      }
      clearHashFragment()
    }

    await validateStoredAuth()
  }

  async function validateStoredAuth() {
    if (!authToken || !currentUser) {
      return
    }

    try {
      const user = await apiRequest('/api/me', { auth: true })
      if (user && typeof user === 'object') {
        currentUser = user
        localStorage.setItem(USER_KEY, JSON.stringify(currentUser))
        void fetchUserSessions()
      }
    } catch (error) {
      if (!isUnauthorizedApiError(error)) {
        return
      }

      if (route.name === 'home') {
        homeMessage = 'Saved login expired. Please log in again.'
      } else {
        questionStatus = 'Saved login expired. Please log in again.'
      }
    }
  }

  async function createSession(event) {
    event.preventDefault()
    if (!authToken) {
      createStatus = 'Login first to create a session.'
      return
    }

    const name = createName.trim()
    if (!name) {
      createStatus = 'Name is required.'
      return
    }

    createBusy = true
    createStatus = ''

    try {
      const payload = await apiRequest('/api/sessions', {
        method: 'POST',
        body: JSON.stringify({
          name,
          description: createDescription.trim() || null,
          stream_link: createStreamLink.trim() || null
        }),
        auth: true
      })

      createName = ''
      createDescription = ''
      createStreamLink = ''
      showCreateForm = false
      goto(`/s/${payload.session.public_code}`)
    } catch (error) {
      createStatus = error instanceof Error ? error.message : 'Failed to create session.'
    } finally {
      createBusy = false
    }
  }

  async function startSessionView(code) {
    disconnectSessionEvents()
    clearAutoRefreshDebounce()
    activeSessionCode = code
    updateMode = 'manual'
    updateModeTouched = false
    pendingNewQuestions = 0
    if (!['top', 'new', 'answered'].includes(sessionSort)) {
      sessionSort = 'top'
    }
    localVotes = {}
    hideInteracted = false
    loadInteractedQuestions(code, false)
    await refreshQuestions(code)
    connectSessionEvents(code)
  }

  function clearAutoRefreshDebounce() {
    if (autoRefreshDebounceTimer !== null) {
      window.clearTimeout(autoRefreshDebounceTimer)
      autoRefreshDebounceTimer = null
    }
  }

  function scheduleAutoRefresh() {
    if (updateMode !== 'auto' || !activeSessionCode) {
      return
    }
    if (autoRefreshDebounceTimer !== null) {
      return
    }

    autoRefreshDebounceTimer = window.setTimeout(() => {
      autoRefreshDebounceTimer = null
      refreshQuestions(activeSessionCode)
    }, AUTO_REFRESH_DEBOUNCE_MS)
  }

  function connectSessionEvents(code) {
    disconnectSessionEvents()
    const url = `${apiBase}/api/sessions/${encodeURIComponent(code)}/events`
    const source = new EventSource(url)

    source.onopen = () => {
      sseConnected = true
    }
    source.onmessage = (event) => {
      handleSessionEvent(event.data)
    }
    source.onerror = () => {
      sseConnected = false
    }

    eventSource = source
  }

  function disconnectSessionEvents() {
    if (eventSource !== null) {
      eventSource.close()
      eventSource = null
    }
    sseConnected = false
  }

  function handleSessionEvent(rawData) {
    if (route.name !== 'session' || !activeSessionCode) {
      return
    }

    let payload = null
    try {
      payload = JSON.parse(rawData)
    } catch {
      return
    }

    if (payload.kind === 'resync') {
      void refreshQuestions(activeSessionCode)
      return
    }

    if (updateMode === 'auto') {
      scheduleAutoRefresh()
      return
    }

    if (payload.kind === 'question_created') {
      pendingNewQuestions += 1
    }
  }

  function applyDefaultUpdateMode(isOwner, isLoggedIn) {
    if (updateModeTouched) {
      return
    }

    const defaultMode = isOwner || !isLoggedIn ? 'auto' : 'manual'
    if (updateMode !== defaultMode) {
      updateMode = defaultMode
    }
  }

  async function refreshQuestions(code = route.code) {
    if (!code) {
      return
    }

    loadingQuestions = true
    try {
      const payload = await apiRequest(
        `/api/sessions/${encodeURIComponent(code)}/questions?sort=${encodeURIComponent(sessionSort)}`
      )
      sessionData = payload.session
      questions = payload.questions
      rememberCurrentUserAuthoredQuestions(code)
      sessionError = ''
      pendingNewQuestions = 0
    } catch (error) {
      sessionError = error instanceof Error ? error.message : 'Failed to load questions.'
    } finally {
      loadingQuestions = false
    }
  }

  async function changeSort(sort) {
    if (sessionSort === sort) {
      return
    }

    sessionSort = sort
    await refreshQuestions()
  }

  function setUpdateMode(mode) {
    if (mode !== 'manual' && mode !== 'auto') {
      return
    }
    if (updateMode === mode) {
      return
    }

    updateMode = mode
    updateModeTouched = true
    if (updateMode === 'auto' && pendingNewQuestions > 0) {
      scheduleAutoRefresh()
    }
  }

  async function submitQuestion(event) {
    event.preventDefault()

    if (!authToken) {
      questionStatus = 'Login first to submit a question.'
      return
    }

    const text = questionText.trim()
    if (!text) {
      questionStatus = 'Question cannot be empty.'
      return
    }

    if (text.length > 300) {
      questionStatus = 'Question max length is 300 characters.'
      return
    }

    questionBusy = true
    questionStatus = ''

    try {
      const payload = await apiRequest(`/api/sessions/${encodeURIComponent(route.code)}/questions`, {
        method: 'POST',
        body: JSON.stringify({ text }),
        auth: true
      })
      addInteractedQuestion(payload?.id)

      questionText = ''
      questionStatus = 'Question added.'
      await refreshQuestions()
    } catch (error) {
      questionStatus = error instanceof Error ? error.message : 'Failed to add question.'
    } finally {
      questionBusy = false
    }
  }

  async function vote(questionId, value) {
    if (!authToken) {
      questionStatus = 'Login first to vote.'
      return
    }

    voteBusy.add(questionId)
    voteBusy = new Set(voteBusy)

    try {
      const payload = await apiRequest(`/api/questions/${questionId}/vote`, {
        method: 'POST',
        body: JSON.stringify({ value }),
        auth: true
      })

      localVotes = {
        ...localVotes,
        [questionId]: payload.user_vote
      }
      addInteractedQuestion(questionId)

      const index = questions.findIndex((question) => question.id === questionId)
      if (index >= 0) {
        questions[index] = {
          ...questions[index],
          score: payload.score
        }
        questions = [...questions]
      }
    } catch (error) {
      questionStatus = error instanceof Error ? error.message : 'Failed to vote.'
    } finally {
      voteBusy.delete(questionId)
      voteBusy = new Set(voteBusy)
    }
  }

  $: admin = !!currentUser && !!sessionData && currentUser.id === sessionData.owner_user_id

  $: viewerCanInteract =
    !!currentUser &&
    !!sessionData &&
    sessionData.is_active === 1 &&
    currentUser.id !== sessionData.owner_user_id

  async function moderateQuestion(questionId, action) {
    if (!authToken) {
      questionStatus = 'Login first.'
      return
    }

    if (!admin) {
      questionStatus = 'Only session owner can moderate questions.'
      return
    }

    moderateBusy.add(questionId)
    moderateBusy = new Set(moderateBusy)

    try {
      const payload = await apiRequest(`/api/questions/${questionId}/moderate`, {
        method: 'POST',
        body: JSON.stringify({ action }),
        auth: true
      })

      if (payload.deleted) {
        questionStatus = 'Question deleted.'
        await refreshQuestions()
      } else if (payload.banned) {
        questionStatus = 'User banned.'
        await refreshQuestions()
      } else if (payload.question) {
        if (action === 'answer') {
          questionStatus = 'Question is now in progress.'
        } else if (action === 'finish_answering') {
          questionStatus = 'Question moved to answered.'
        } else if (action === 'reject') {
          questionStatus = 'Question rejected.'
        } else {
          questionStatus = 'Question updated.'
        }
        await refreshQuestions()
      }
    } catch (error) {
      questionStatus = error instanceof Error ? error.message : 'Moderation failed.'
    } finally {
      moderateBusy.delete(questionId)
      moderateBusy = new Set(moderateBusy)
    }
  }

  async function updateSessionSettings(event) {
    event.preventDefault()
    if (!authToken) return

    const name = settingsName.trim()
    if (!name) {
      settingsStatus = 'Name is required.'
      return
    }

    settingsBusy = true
    settingsStatus = ''

    try {
      const payload = await apiRequest(`/api/sessions/${encodeURIComponent(route.code)}`, {
        method: 'PUT',
        body: JSON.stringify({
          name,
          description: settingsDescription.trim() || null,
          stream_link: settingsStreamLink.trim() || null
        }),
        auth: true
      })
      sessionData = payload
      settingsStatus = 'Settings saved.'
      showSessionSettings = false
    } catch (error) {
      settingsStatus = error instanceof Error ? error.message : 'Failed to save settings.'
    } finally {
      settingsBusy = false
    }
  }

  function openSessionSettings() {
    settingsName = sessionData?.name ?? ''
    settingsDescription = sessionData?.description ?? ''
    settingsStreamLink = sessionData?.stream_link ?? ''
    settingsStatus = ''
    showSessionSettings = true
  }

  async function stopSession() {
    stoppingSession = true
    try {
      const payload = await apiRequest(`/api/sessions/${encodeURIComponent(route.code)}/stop`, {
        method: 'POST',
        auth: true
      })
      sessionData = payload
      showSessionSettings = false
      showStopConfirm = false
    } catch (error) {
      settingsStatus = error instanceof Error ? error.message : 'Failed to stop session.'
    } finally {
      stoppingSession = false
    }
  }

  async function deleteSession(code) {
    deletingSessionCode = code
    try {
      await apiRequest(`/api/sessions/${encodeURIComponent(code)}`, {
        method: 'DELETE',
        auth: true
      })
      userSessions = userSessions.filter((s) => s.public_code !== code)
    } catch (error) {
      homeMessage = error instanceof Error ? error.message : 'Failed to delete session.'
    } finally {
      deletingSessionCode = null
    }
  }

  function formatTime(unixTime) {
    const date = new Date(unixTime * 1000)
    const now = new Date()
    const diffMs = now - date
    const diffMin = Math.floor(diffMs / 60000)
    const diffHr = Math.floor(diffMs / 3600000)

    if (diffMin < 1) return 'just now'
    if (diffMin < 60) return `${diffMin}m ago`
    if (diffHr < 24) return `${diffHr}h ago`
    return date.toLocaleDateString()
  }

  function userInitial(nickname) {
    return (nickname || '?')[0].toUpperCase()
  }
</script>

<div class="app-shell">
  {#if route.name === 'home'}
    <!-- HOME PAGE -->
    <div class="app-body" style="display: grid; place-items: center; min-height: 100vh;">
      <div class="card card-centered">
        <span class="label-tag">QStream</span>
        <h1>Question Room</h1>
        <p class="subtitle">
          Collect and rank audience questions in real time during your stream.
        </p>

        {#if currentUser}
          <div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: 16px;">
            <div class="user-pill">
              <span class="user-avatar">{userInitial(currentUser.nickname)}</span>
              {currentUser.nickname}
            </div>
            <button type="button" class="btn btn-ghost" on:click={logout}>Log out</button>
          </div>

          <!-- Session list -->
          {#if loadingSessions}
            <p class="text-sm text-secondary" style="margin-bottom: 12px;">Loading sessions...</p>
          {:else if userSessions.length > 0}
            <div style="display: flex; flex-direction: column; gap: 8px; margin-bottom: 16px;">
              {#each userSessions as session}
                <div style="display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 10px 12px; background: var(--color-surface-2, #f5f5f5); border-radius: 8px;">
                  <div style="min-width: 0; flex: 1;">
                    <div style="display: flex; align-items: center; gap: 6px; flex-wrap: wrap;">
                      <span style="font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;">{session.name}</span>
                      {#if session.is_active === 1}
                        <span class="badge badge-answering" style="font-size: 11px; padding: 1px 6px;">Active</span>
                      {:else}
                        <span class="badge badge-rejected" style="font-size: 11px; padding: 1px 6px;">Stopped</span>
                      {/if}
                    </div>
                    {#if session.description}
                      <div class="text-sm text-secondary" style="white-space: nowrap; overflow: hidden; text-overflow: ellipsis;">{session.description}</div>
                    {/if}
                  </div>
                  <div style="display: flex; gap: 6px; flex-shrink: 0;">
                    <a class="btn btn-secondary btn-sm" href={`/s/${session.public_code}`}>Open</a>
                    {#if session.is_active === 0}
                      <ConfirmButton
                        class="btn btn-danger btn-sm"
                        label="Delete"
                        confirmLabel="Confirm delete"
                        disabled={deletingSessionCode === session.public_code}
                        on:confirm={() => deleteSession(session.public_code)}
                      />
                    {/if}
                  </div>
                </div>
              {/each}
            </div>
          {/if}

          <!-- Create session -->
          {#if showCreateForm}
            <form on:submit={createSession} style="display: flex; flex-direction: column; gap: 10px; margin-bottom: 8px;">
              <div>
                <label for="create-name" class="text-sm" style="display: block; margin-bottom: 4px;">Name <span style="color: var(--color-danger)">*</span></label>
                <input
                  id="create-name"
                  type="text"
                  maxlength="100"
                  bind:value={createName}
                  placeholder="My Stream Q&A"
                  style="width: 100%; box-sizing: border-box;"
                />
              </div>
              <div>
                <label for="create-description" class="text-sm" style="display: block; margin-bottom: 4px;">Description</label>
                <textarea
                  id="create-description"
                  maxlength="500"
                  bind:value={createDescription}
                  placeholder="Short description shown to viewers"
                  rows="2"
                  style="width: 100%; box-sizing: border-box;"
                ></textarea>
              </div>
              <div>
                <label for="create-stream-link" class="text-sm" style="display: block; margin-bottom: 4px;">Stream link</label>
                <input
                  id="create-stream-link"
                  type="url"
                  maxlength="500"
                  bind:value={createStreamLink}
                  placeholder="https://twitch.tv/yourname"
                  style="width: 100%; box-sizing: border-box;"
                />
              </div>
              <div style="display: flex; gap: 8px; align-items: center; flex-wrap: wrap;">
                <button type="submit" class="btn btn-primary" disabled={createBusy}>
                  {createBusy ? 'Creating...' : 'Create'}
                </button>
                <button type="button" class="btn btn-ghost" on:click={() => { showCreateForm = false; createStatus = '' }}>
                  Cancel
                </button>
                {#if createStatus}
                  <span class="text-sm" style="color: var(--color-danger);">{createStatus}</span>
                {/if}
              </div>
            </form>
          {:else}
            <button type="button" class="btn btn-primary" on:click={() => { showCreateForm = true; createStatus = '' }}>
              New session
            </button>
          {/if}
        {:else}
          <LoginPanel
            {apiBase}
            title="Get started"
            subtitle="Continue with Google to participate."
            submitLabel="Continue with Google"
            returnTo="/"
          />
        {/if}

        {#if homeMessage}
          <p class="msg msg-info">{homeMessage}</p>
        {/if}
      </div>
    </div>

  {:else}
    <!-- SESSION PAGE -->
    <header class="app-header">
      <div class="app-header-inner">
        <a class="app-logo" href="/" on:click|preventDefault={() => goto('/')}>QStream</a>

        <div class="app-header-right">
          <span class="text-sm text-secondary">
            <span class="status-dot" class:connected={sseConnected} class:disconnected={!sseConnected}></span>
            {sseConnected ? 'Live' : 'Reconnecting'}
          </span>

          {#if currentUser}
            <div class="user-pill">
              <span class="user-avatar">{userInitial(currentUser.nickname)}</span>
              {currentUser.nickname}
            </div>
            <button type="button" class="btn btn-ghost btn-sm" on:click={logout}>Log out</button>
          {:else}
            <button type="button" class="btn btn-secondary btn-sm" on:click={() => (showSessionLogin = !showSessionLogin)}>
              {showSessionLogin ? 'Cancel' : 'Log in'}
            </button>
          {/if}
        </div>
      </div>
    </header>

    <div class="app-body">
      <div style="margin-bottom: 20px;">
        <span class="label-tag">Session</span>
        <h1>{sessionData?.name || 'Session'}</h1>
        {#if sessionData?.description}
          <p class="text-sm text-secondary" style="margin-top: 4px;">{sessionData.description}</p>
        {/if}
        {#if sessionData?.stream_link}
          <p class="text-sm" style="margin-top: 4px;">
            <a href={sessionData.stream_link} target="_blank" rel="noopener noreferrer">Watch stream</a>
          </p>
        {/if}
        <div style="margin-top: 8px; display: flex; align-items: center; gap: 8px; flex-wrap: wrap;">
          {#if admin}
            <p class="text-sm text-secondary">You own this session. Use moderation controls on each question.</p>
            <button type="button" class="btn btn-ghost btn-sm" on:click={openSessionSettings}>Settings</button>
          {:else if currentUser}
            <p class="text-sm text-secondary">Ask questions and vote on others.</p>
          {:else}
            <p class="text-sm text-secondary">Log in to ask questions and vote.</p>
          {/if}
        </div>
      </div>

      {#if sessionData && sessionData.is_active === 0}
        <div class="msg msg-info" style="margin-bottom: 16px;">
          This session has ended. Questions are no longer accepted.
        </div>
      {/if}

      {#if admin && showSessionSettings}
        <div class="card section-gap" style="margin-bottom: 16px;">
          <h3 style="margin: 0 0 12px;">Session settings</h3>
          <form on:submit={updateSessionSettings}>
            <div style="display: flex; flex-direction: column; gap: 10px;">
              <div>
                <label for="settings-name" class="text-sm" style="display: block; margin-bottom: 4px;">Name <span style="color: var(--color-danger)">*</span></label>
                <input
                  id="settings-name"
                  type="text"
                  maxlength="100"
                  bind:value={settingsName}
                  placeholder="My Stream Q&A"
                  style="width: 100%; box-sizing: border-box;"
                />
              </div>
              <div>
                <label for="settings-description" class="text-sm" style="display: block; margin-bottom: 4px;">Description</label>
                <textarea
                  id="settings-description"
                  maxlength="500"
                  bind:value={settingsDescription}
                  placeholder="Short description shown to viewers"
                  rows="2"
                  style="width: 100%; box-sizing: border-box;"
                ></textarea>
              </div>
              <div>
                <label for="settings-stream-link" class="text-sm" style="display: block; margin-bottom: 4px;">Stream link</label>
                <input
                  id="settings-stream-link"
                  type="url"
                  maxlength="500"
                  bind:value={settingsStreamLink}
                  placeholder="https://twitch.tv/yourname"
                  style="width: 100%; box-sizing: border-box;"
                />
              </div>
              <div style="display: flex; gap: 8px; align-items: center;">
                <button type="submit" class="btn btn-primary btn-sm" disabled={settingsBusy}>
                  {settingsBusy ? 'Saving...' : 'Save'}
                </button>
                <button type="button" class="btn btn-ghost btn-sm" on:click={() => { showSessionSettings = false; showStopConfirm = false }}>
                  Cancel
                </button>
                {#if settingsStatus}
                  <span class="text-sm" style="color: var(--color-danger);">{settingsStatus}</span>
                {/if}
              </div>

              {#if sessionData && sessionData.is_active === 1}
                <div style="border-top: 1px solid var(--color-border, #e0e0e0); margin-top: 14px; padding-top: 14px;">
                  <ConfirmButton
                    class="btn btn-danger btn-sm"
                    label="Stop session"
                    confirmLabel="Confirm stop"
                    disabled={stoppingSession}
                    on:confirm={stopSession}
                  />
                </div>
              {/if}
            </div>
          </form>
        </div>
      {/if}

      {#if !currentUser && showSessionLogin}
        <div class="card section-gap" style="margin-bottom: 16px;">
          <LoginPanel
            {apiBase}
            title="Log in to interact"
            subtitle="Continue with Google to ask questions and vote."
            submitLabel="Continue with Google"
            returnTo={`/s/${route.code}`}
          />
        </div>
      {/if}

      {#if viewerCanInteract}
        <div class="q-form">
          <form on:submit={submitQuestion}>
            <textarea
              maxlength="300"
              bind:value={questionText}
              placeholder="Type your question..."
            ></textarea>
            <div class="q-form-footer">
              <span class="char-count">{questionText.trim().length} / 300</span>
              <button type="submit" class="btn btn-primary btn-sm" disabled={questionBusy}>
                {questionBusy ? 'Sending...' : 'Ask'}
              </button>
            </div>
          </form>
        </div>
      {/if}

      {#if questionStatus}
        <p class="msg {questionStatus.includes('failed') || questionStatus.includes('Failed') || questionStatus.includes('error') || questionStatus.includes('Error') || questionStatus.includes('cannot') || questionStatus.includes('Cannot') || questionStatus.includes('Only') || questionStatus.includes('Login first') || questionStatus.includes('max length') ? 'msg-error' : 'msg-success'}">{questionStatus}</p>
      {/if}

      {#if sessionError}
        <p class="msg msg-error">{sessionError}</p>
      {/if}

      <!-- Toolbar -->
      <div class="toolbar section-gap">
        <div class="tab-bar">
          <button
            type="button"
            class="tab"
            class:active={sessionSort === 'top'}
            on:click={() => changeSort('top')}
          >Top</button>
          <button
            type="button"
            class="tab"
            class:active={sessionSort === 'new'}
            on:click={() => changeSort('new')}
          >New</button>
          <button
            type="button"
            class="tab"
            class:active={sessionSort === 'answered'}
            on:click={() => changeSort('answered')}
          >Answered</button>
        </div>

        <div class="toolbar-spacer"></div>

        {#if viewerCanInteract}
          <label class="toggle-label">
            <input type="checkbox" bind:checked={hideInteracted} />
            Hide voted
          </label>
        {/if}

        <div class="tab-bar">
          <button
            type="button"
            class="tab"
            class:active={updateMode === 'auto'}
            on:click={() => setUpdateMode('auto')}
          >Auto</button>
          <button
            type="button"
            class="tab"
            class:active={updateMode === 'manual'}
            on:click={() => setUpdateMode('manual')}
          >Manual</button>
        </div>

        <button type="button" class="btn btn-secondary btn-sm" on:click={() => refreshQuestions()}>
          Refresh
          {#if pendingNewQuestions > 0}
            <span class="pending-count">{pendingNewQuestions}</span>
          {/if}
        </button>
      </div>

      <!-- Question list -->
      {#if loadingQuestions}
        <div class="empty-state">
          <p class="text-secondary">Loading...</p>
        </div>
      {/if}

      <div class="q-list">
        {#if visibleQuestions.length === 0 && !loadingQuestions}
          <div class="empty-state">
            <div class="empty-state-icon">?</div>
            <p>{hideInteracted && currentUser ? 'All questions filtered.' : 'No questions yet.'}</p>
          </div>
        {/if}

        {#each visibleQuestions as item}
          <article class="q-card" class:answering={item.is_answering === 1} class:answered={item.is_answered === 1} class:rejected={item.is_rejected === 1}>
            <div class="q-vote-col">
              {#if viewerCanInteract}
                <button
                  type="button"
                  class="q-vote-btn"
                  class:upvoted={localVotes[item.id] === 1}
                  on:click={() => vote(item.id, 1)}
                  disabled={voteBusy.has(item.id) || item.is_answered === 1 || item.is_answering === 1 || item.is_rejected === 1}
                  title="Upvote"
                >&#9650;</button>
              {/if}

              <span class="q-score">{item.score}</span>

              {#if viewerCanInteract}
                <button
                  type="button"
                  class="q-vote-btn"
                  class:downvoted={localVotes[item.id] === -1}
                  on:click={() => vote(item.id, -1)}
                  disabled={voteBusy.has(item.id) || item.is_answered === 1 || item.is_answering === 1 || item.is_rejected === 1}
                  title="Downvote"
                >&#9660;</button>
              {/if}
            </div>

            <div class="q-body-col">
              {#if item.is_answering === 1}
                <span class="badge badge-answering">Answering</span>
              {:else if item.is_answered === 1}
                <span class="badge badge-answered">Answered</span>
              {:else if item.is_rejected === 1}
                <span class="badge badge-rejected">Rejected</span>
              {/if}

              <p class="q-text">{item.body}</p>

              <div class="q-meta">
                <span>@{item.author_nickname}</span>
                <span class="q-meta-sep">&middot;</span>
                <span>{formatTime(item.created_at)}</span>
                <span class="q-meta-sep">&middot;</span>
                <span>{item.votes_count} vote{item.votes_count === 1 ? '' : 's'}</span>
              </div>

              {#if admin}
                <div class="q-actions" style="display: flex; align-items: center; gap: 6px;">
                  {#if item.is_answered === 0}
                    <button
                      type="button"
                      class="btn btn-secondary btn-sm"
                      on:click={() => moderateQuestion(item.id, item.is_answering === 1 ? 'finish_answering' : 'answer')}
                      disabled={moderateBusy.has(item.id)}
                    >{item.is_answering === 1 ? 'Done' : 'Answer'}</button>
                  {/if}

                  {#if item.is_answered === 0 && item.is_rejected === 0}
                    <button
                      type="button"
                      class="btn btn-secondary btn-sm"
                      on:click={() => moderateQuestion(item.id, 'reject')}
                      disabled={moderateBusy.has(item.id)}
                    >Reject</button>
                  {/if}

                  <div style="margin-left: auto; display: flex; gap: 6px;">
                    <ConfirmButton
                      class="btn btn-danger btn-sm"
                      label="Delete"
                      confirmLabel="Confirm delete"
                      disabled={moderateBusy.has(item.id)}
                      on:confirm={() => moderateQuestion(item.id, 'delete')}
                    />
                    <ConfirmButton
                      class="btn btn-danger btn-sm"
                      label="Ban"
                      confirmLabel="Confirm ban"
                      disabled={moderateBusy.has(item.id)}
                      on:confirm={() => moderateQuestion(item.id, 'ban')}
                    />
                  </div>
                </div>
              {/if}
            </div>
          </article>
        {/each}
      </div>
    </div>
  {/if}
</div>
