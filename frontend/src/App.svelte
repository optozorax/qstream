<script>
  import { onDestroy, onMount } from 'svelte'
  import LoginPanel from './lib/LoginPanel.svelte'

  const apiBase = import.meta.env.VITE_API_BASE_URL ?? 'http://localhost:3000'
  const siteKey = import.meta.env.VITE_HCAPTCHA_SITE_KEY ?? ''

  const AUTH_TOKEN_KEY = 'qstream_auth_token'
  const USER_KEY = 'qstream_user'
  const SESSION_CODE_KEY = 'qstream_current_session_code'
  const OWN_SESSION_CODE_KEY = 'qstream_own_session_code'
  const INTERACTED_QUESTIONS_PREFIX = 'qstream_interacted_questions'

  let route = parseRoute(window.location.pathname)
  let authToken = localStorage.getItem(AUTH_TOKEN_KEY) ?? ''
  let currentUser = parseStoredUser(localStorage.getItem(USER_KEY))
  let storedSessionCode = localStorage.getItem(SESSION_CODE_KEY) ?? ''
  let ownSessionCode = localStorage.getItem(OWN_SESSION_CODE_KEY) ?? ''

  let homeMessage = ''
  let creatingSession = false
  let showSessionLogin = false

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

  function setSessionCode(code) {
    storedSessionCode = code
    localStorage.setItem(SESSION_CODE_KEY, code)
  }

  function setOwnSessionCode(code) {
    ownSessionCode = code
    if (code) {
      localStorage.setItem(OWN_SESSION_CODE_KEY, code)
    } else {
      localStorage.removeItem(OWN_SESSION_CODE_KEY)
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

  function setAuth(payload) {
    authToken = payload.auth_token
    currentUser = payload.user

    localStorage.setItem(AUTH_TOKEN_KEY, authToken)
    localStorage.setItem(USER_KEY, JSON.stringify(currentUser))

    if (payload.session?.public_code) {
      setOwnSessionCode(payload.session.public_code)
      setSessionCode(payload.session.public_code)
    } else {
      setOwnSessionCode('')
    }
  }

  function logout() {
    authToken = ''
    currentUser = null
    localVotes = {}
    interactedQuestionIds = new Set()
    hideInteracted = false
    setOwnSessionCode('')
    localStorage.removeItem(AUTH_TOKEN_KEY)
    localStorage.removeItem(USER_KEY)
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
      throw new Error(payload.error ?? `Request failed with status ${response.status}`)
    }

    return payload
  }

  async function createSession() {
    if (!authToken) {
      homeMessage = 'Login first to create a session.'
      return
    }

    creatingSession = true
    homeMessage = ''

    try {
      const payload = await apiRequest('/api/sessions', {
        method: 'POST',
        body: JSON.stringify({}),
        auth: true
      })

      setOwnSessionCode(payload.session.public_code)
      setSessionCode(payload.session.public_code)
      homeMessage = payload.created
        ? 'Session created. Opening session page...'
        : 'Session already exists. Opening session page...'
      goto(`/s/${payload.session.public_code}`)
    } catch (error) {
      homeMessage = error instanceof Error ? error.message : 'Failed to create session.'
    } finally {
      creatingSession = false
    }
  }

  function handleLoginSuccess(event) {
    const payload = event.detail
    setAuth(payload)

    if (route.name === 'home' && payload.session?.public_code) {
      setSessionCode(payload.session.public_code)
    }

    if (route.name === 'session') {
      showSessionLogin = false
      questionStatus = 'Logged in. You can ask and vote now.'
      localVotes = {}
      loadInteractedQuestions(route.code)
    }
  }

  async function startSessionView(code) {
    disconnectSessionEvents()
    clearAutoRefreshDebounce()
    activeSessionCode = code
    setSessionCode(code)
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
      setSessionCode(code)
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

  function isAdmin() {
    return !!currentUser && !!sessionData && currentUser.id === sessionData.owner_user_id
  }

  function canUseViewerInteractions() {
    if (!currentUser) {
      return false
    }
    if (route.name === 'session' && ownSessionCode && route.code === ownSessionCode) {
      return false
    }
    if (sessionData && currentUser.id === sessionData.owner_user_id) {
      return false
    }
    return true
  }

  async function moderateQuestion(questionId, action) {
    if (!authToken) {
      questionStatus = 'Login first.'
      return
    }

    if (!isAdmin()) {
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
        questions = questions.filter((question) => question.id !== questionId)
        questionStatus = 'Question deleted.'
      } else if (payload.question) {
        if (action === 'answer') {
          questionStatus = 'Question is now in progress.'
        } else if (action === 'finish_answering') {
          questionStatus = 'Question moved to answered.'
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

  function formatTime(unixTime) {
    const date = new Date(unixTime * 1000)
    return date.toLocaleString()
  }
</script>

<main>
  {#if route.name === 'home'}
    <section class="panel home-panel">
      <p class="eyebrow">QStream</p>
      <h1>Streamer Question Room</h1>
      <p class="hint">
        Login with nickname + hCaptcha, then create one session and share its link with viewers.
      </p>

      {#if currentUser}
        <div class="auth-line">
          <span>Logged in as <strong>{currentUser.nickname}</strong></span>
          <button type="button" class="ghost" on:click={logout}>Logout</button>
        </div>
      {:else}
        <LoginPanel
          {apiBase}
          {siteKey}
          title="Login"
          subtitle="Use your nickname and pass hCaptcha."
          submitLabel="Login"
          on:success={handleLoginSuccess}
        />
      {/if}

      {#if currentUser}
        <div class="actions">
          <button type="button" on:click={createSession} disabled={creatingSession}>
            {creatingSession ? 'Creating...' : 'Create'}
          </button>

          {#if storedSessionCode}
            <a class="link-button" href={`/s/${storedSessionCode}`}>Open current session</a>
          {/if}
        </div>
      {/if}

      {#if homeMessage}
        <p class="message info">{homeMessage}</p>
      {/if}
    </section>
  {:else}
    <section class="panel session-panel">
      <div class="header-row">
        <div>
          <p class="eyebrow">Public Session</p>
          <h1>Session {route.code}</h1>
        </div>
        <a class="ghost" href="/">Back to main</a>
      </div>

      <p class="hint">
        This is the public question list. Sort and live auto-updates are available for everyone. SSE: {sseConnected ? 'connected' : 'reconnecting'}.
      </p>

      <div class="session-tools">
        <div class="tabs">
          <button
            type="button"
            class:active={sessionSort === 'top'}
            on:click={() => changeSort('top')}
          >
            Top
          </button>
          <button
            type="button"
            class:active={sessionSort === 'new'}
            on:click={() => changeSort('new')}
          >
            New
          </button>
          <button
            type="button"
            class:active={sessionSort === 'answered'}
            on:click={() => changeSort('answered')}
          >
            Answered
          </button>
        </div>

        <div class="update-controls">
          <button
            type="button"
            class:active={updateMode === 'manual'}
            on:click={() => setUpdateMode('manual')}
          >
            Manual
          </button>
          <button
            type="button"
            class:active={updateMode === 'auto'}
            on:click={() => setUpdateMode('auto')}
          >
            Auto (live)
          </button>
          <button type="button" class="ghost" on:click={() => refreshQuestions()}>
            {pendingNewQuestions > 0 ? `Update now (${pendingNewQuestions} new)` : 'Update now'}
          </button>
          {#if canUseViewerInteractions()}
            <label class="interacted-toggle">
              <input type="checkbox" bind:checked={hideInteracted} />
              <span>Hide interacted</span>
            </label>
          {/if}
        </div>

        {#if currentUser}
          <div class="auth-line compact">
            <span><strong>{currentUser.nickname}</strong></span>
            <button type="button" class="ghost" on:click={logout}>Logout</button>
          </div>
        {:else}
          <button type="button" class="ghost" on:click={() => (showSessionLogin = !showSessionLogin)}>
            {showSessionLogin ? 'Close login' : 'Login'}
          </button>
        {/if}
      </div>

      {#if isAdmin()}
        <p class="hint">Admin mode: use `Answer` to start, then `Finish answering` to move question to answered. Auto updates are default.</p>
      {:else if !currentUser}
        <p class="hint">Guest mode: auto updates are default.</p>
      {:else}
        <p class="hint">Viewer mode: manual updates are default.</p>
      {/if}

      {#if !currentUser && showSessionLogin}
        <LoginPanel
          {apiBase}
          {siteKey}
          title="Login to interact"
          subtitle="After login you can ask questions and vote."
          submitLabel="Login"
          on:success={handleLoginSuccess}
        />
      {/if}

      {#if canUseViewerInteractions()}
        <form class="question-form" on:submit={submitQuestion}>
          <label for="question">Ask a question</label>
          <textarea
            id="question"
            maxlength="300"
            bind:value={questionText}
            placeholder="Plain text only, max 300 chars"
          ></textarea>
          <div class="question-form-bottom">
            <span>{questionText.trim().length}/300</span>
            <button type="submit" disabled={questionBusy}>
              {questionBusy ? 'Sending...' : 'Send question'}
            </button>
          </div>
        </form>
      {:else if isAdmin()}
        <p class="hint">Owner mode: question submission is disabled for the session owner.</p>
      {/if}

      {#if questionStatus}
        <p class="message info">{questionStatus}</p>
      {/if}

      {#if sessionError}
        <p class="message error">{sessionError}</p>
      {/if}

      {#if loadingQuestions}
        <p class="hint">Loading questions...</p>
      {/if}

      <div class="question-list">
        {#if visibleQuestions.length === 0 && !loadingQuestions}
          <p class="hint">{hideInteracted && currentUser ? 'No questions left after your filter.' : 'No questions yet.'}</p>
        {/if}

        {#each visibleQuestions as item}
          <article class="question-item">
            <div class="question-meta">
              <span>@{item.author_nickname}</span>
              <span>{formatTime(item.created_at)}</span>
            </div>
            {#if item.is_answering === 1}
              <div class="answering-badge">Answer in progress</div>
            {:else if item.is_answered === 1}
              <div class="answered-badge">Answered</div>
            {/if}
            <p class="question-body">{item.body}</p>
            <div class="question-footer">
              <strong>Score: {item.score}</strong>
              <span>{item.votes_count} votes</span>

              {#if canUseViewerInteractions()}
                <div class="vote-actions">
                  <button
                    type="button"
                    class:active={localVotes[item.id] === 1}
                    on:click={() => vote(item.id, 1)}
                    disabled={voteBusy.has(item.id) || item.is_answered === 1 || item.is_answering === 1}
                  >
                    Like
                  </button>
                  <button
                    type="button"
                    class:active={localVotes[item.id] === -1}
                    on:click={() => vote(item.id, -1)}
                    disabled={voteBusy.has(item.id) || item.is_answered === 1 || item.is_answering === 1}
                  >
                    Dislike
                  </button>
                </div>
              {/if}

              {#if isAdmin()}
                <div class="admin-actions">
                  {#if item.is_answered === 0}
                    <button
                      type="button"
                      class="ghost"
                      on:click={() => moderateQuestion(item.id, item.is_answering === 1 ? 'finish_answering' : 'answer')}
                      disabled={moderateBusy.has(item.id)}
                    >
                      {item.is_answering === 1 ? 'Finish answering' : 'Answer'}
                    </button>
                  {/if}
                  <button
                    type="button"
                    class="ghost danger"
                    on:click={() => moderateQuestion(item.id, 'delete')}
                    disabled={moderateBusy.has(item.id)}
                  >
                    Delete
                  </button>
                </div>
              {/if}
            </div>
          </article>
        {/each}
      </div>
    </section>
  {/if}
</main>
