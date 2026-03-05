<script>
  import { onDestroy, onMount } from 'svelte'
  import LoginPanel from './lib/LoginPanel.svelte'
  import ConfirmButton from './lib/ConfirmButton.svelte'
  import SessionDetailsFields from './lib/SessionDetailsFields.svelte'
  import Spinner from './lib/Spinner.svelte'
  import Notifications from './lib/Notifications.svelte'

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
  let settingsThreshold = 5
  let stoppingSession = false

  // Timecodes panel (admin, ended session)
  let timecodeDay = 1
  let timecodeMonth = 1
  let timecodeYear = 2024
  let timecodeHour = 0
  let timecodeMinute = 0
  let timecodeSecond = 0
  let timecodeQuestions = []
  let timecodeLoading = false
  let timecodeCopied = false
  let timecodeQuestionsLoaded = false
  let bannedUsers = []
  let loadingBans = false
  let unbanningUserId = null
  let showBannedUsers = false

  // Home page: deleting a session
  let deletingSessionCode = null

  let sessionSort = 'top'
  let loadedSessionSort = 'top'
  let sessionData = null
  let questions = []
  let loadingQuestions = false
  let viewerIsBanned = false

  let notifHistory = []
  let activeToasts = []
  let notifCounter = 0
  function addNotification(msg, type = 'info') {
    const id = ++notifCounter
    const entry = { id, msg, type, time: Date.now() }
    notifHistory = [entry, ...notifHistory]
    activeToasts = [...activeToasts, entry]
    setTimeout(() => { activeToasts = activeToasts.filter(n => n.id !== id) }, 3000)
  }

  let questionText = ''
  let questionBusy = false
  let newQuestionId = null
  let newQuestionTimer = null
  let questionCooldownUntil = 0
  let questionCooldownRemaining = 0
  let questionCooldownTimer = null

  function startQuestionCooldown(seconds = 60) {
    if (seconds <= 0) return
    questionCooldownUntil = nowUnix() + seconds
    questionCooldownRemaining = seconds
    if (questionCooldownTimer !== null) clearInterval(questionCooldownTimer)
    questionCooldownTimer = setInterval(() => {
      const remaining = questionCooldownUntil - nowUnix()
      if (remaining <= 0) {
        questionCooldownRemaining = 0
        clearInterval(questionCooldownTimer)
        questionCooldownTimer = null
      } else {
        questionCooldownRemaining = remaining
      }
    }, 500)
  }
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
  let refreshQuestionsRequestId = 0
  let refreshQuestionsAbortController = null

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
    cancelRefreshQuestions()
    if (questionCooldownTimer !== null) clearInterval(questionCooldownTimer)
    if (newQuestionTimer !== null) clearTimeout(newQuestionTimer)
  })

  $: if (route.name === 'session') {
    if (activeSessionCode !== route.code) {
      startSessionView(route.code)
    }
  } else if (activeSessionCode !== null) {
    activeSessionCode = null
    disconnectSessionEvents()
    clearAutoRefreshDebounce()
    cancelRefreshQuestions()
    sessionData = null
    questions = []
    loadedSessionSort = 'top'
    questionText = ''
    updateMode = 'manual'
    updateModeTouched = false
    pendingNewQuestions = 0
    hideInteracted = false
    interactedQuestionIds = new Set()
    showSessionSettings = false
    settingsStatus = ''
    stoppingSession = false
    newQuestionId = null
    viewerIsBanned = false
    timecodeDay = 1
    timecodeMonth = 1
    timecodeYear = 2024
    timecodeHour = 0
    timecodeMinute = 0
    timecodeSecond = 0
    timecodeQuestions = []
    timecodeLoading = false
    timecodeCopied = false
    timecodeQuestionsLoaded = false
    if (newQuestionTimer !== null) { clearTimeout(newQuestionTimer); newQuestionTimer = null }
    void fetchUserSessions()
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
    bannedUsers = []
    showBannedUsers = false
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
            localVotes = {}
            loadInteractedQuestions(route.code)
          } else {
            void fetchUserSessions()
          }
        }
      } catch {
        logout()
        addNotification('Google login failed. Please retry.', 'error')
        if (route.name !== 'session') {
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

      addNotification('Saved login expired. Please log in again.', 'error')
      if (route.name === 'home') {
        homeMessage = 'Saved login expired. Please log in again.'
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
    cancelRefreshQuestions()
    activeSessionCode = code
    updateMode = 'manual'
    updateModeTouched = false
    pendingNewQuestions = 0
    if (!['top', 'new', 'answered', 'downvoted', 'deleted'].includes(sessionSort)) {
      sessionSort = 'top'
    }
    localVotes = {}
    hideInteracted = false
    loadInteractedQuestions(code, false)
    connectSessionEvents(code)
    await refreshQuestions(code)
  }

  function clearAutoRefreshDebounce() {
    if (autoRefreshDebounceTimer !== null) {
      window.clearTimeout(autoRefreshDebounceTimer)
      autoRefreshDebounceTimer = null
    }
  }

  function cancelRefreshQuestions() {
    refreshQuestionsRequestId += 1
    if (refreshQuestionsAbortController !== null) {
      refreshQuestionsAbortController.abort()
      refreshQuestionsAbortController = null
    }
    loadingQuestions = false
  }

  function isAbortError(error) {
    return !!error && typeof error === 'object' && error.name === 'AbortError'
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

    const requestId = ++refreshQuestionsRequestId
    if (refreshQuestionsAbortController !== null) {
      refreshQuestionsAbortController.abort()
    }
    const controller = new AbortController()
    refreshQuestionsAbortController = controller
    const requestedSort = sessionSort

    loadingQuestions = true
    try {
      const payload = await apiRequest(
        `/api/sessions/${encodeURIComponent(code)}/questions?sort=${encodeURIComponent(requestedSort)}`,
        { auth: true, signal: controller.signal }
      )

      if (requestId !== refreshQuestionsRequestId || route.name !== 'session' || activeSessionCode !== code) {
        return
      }

      sessionData = payload.session
      questions = payload.questions
      loadedSessionSort = payload.sort ?? requestedSort
      const serverVotes = {}
      for (const q of payload.questions) {
        if (q.user_vote !== 0) serverVotes[q.id] = q.user_vote
      }
      localVotes = serverVotes
      viewerIsBanned = payload.viewer_is_banned ?? false
      if (payload.question_cooldown_remaining > 0 && questionCooldownRemaining === 0) {
        startQuestionCooldown(payload.question_cooldown_remaining)
      }
      rememberCurrentUserAuthoredQuestions(code)
      pendingNewQuestions = 0
    } catch (error) {
      if (isAbortError(error) || requestId !== refreshQuestionsRequestId) {
        return
      }
      addNotification(error instanceof Error ? error.message : 'Failed to load questions.', 'error')
    } finally {
      if (requestId === refreshQuestionsRequestId) {
        loadingQuestions = false
        refreshQuestionsAbortController = null
      }
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
      addNotification('Login first to submit a question.', 'error')
      return
    }

    const text = questionText.trim()
    if (!text) {
      addNotification('Question cannot be empty.', 'error')
      return
    }

    if (text.length > 300) {
      addNotification('Question max length is 300 characters.', 'error')
      return
    }
    if (countLineBreaks(text) > 5) {
      addNotification('Question can contain at most 5 line breaks.', 'error')
      return
    }

    questionBusy = true

    try {
      const payload = await apiRequest(`/api/sessions/${encodeURIComponent(route.code)}/questions`, {
        method: 'POST',
        body: JSON.stringify({ text }),
        auth: true
      })
      addInteractedQuestion(payload?.id)

      questionText = ''
      if (newQuestionTimer !== null) clearTimeout(newQuestionTimer)
      newQuestionId = payload?.id ?? null
      startQuestionCooldown()
      await refreshQuestions()
      newQuestionTimer = setTimeout(() => { newQuestionId = null; newQuestionTimer = null }, 3000)
    } catch (error) {
      addNotification(error instanceof Error ? error.message : 'Failed to add question.', 'error')
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
      addNotification(error instanceof Error ? error.message : 'Failed to vote.', 'error')
    } finally {
      voteBusy.delete(questionId)
      voteBusy = new Set(voteBusy)
    }
  }

  $: admin = !!currentUser && !!sessionData && currentUser.id === sessionData.owner_user_id

  $: if (sessionSort === 'deleted' && !admin) {
    sessionSort = 'top'
    if (route.name === 'session' && activeSessionCode) {
      void refreshQuestions(activeSessionCode)
    }
  }

  $: viewerCanInteract =
    !!currentUser &&
    !!sessionData &&
    sessionData.is_active === 1 &&
    currentUser.id !== sessionData.owner_user_id &&
    !viewerIsBanned
  $: activeAnsweringQuestionId =
    questions.find((question) => question.is_answering === 1)?.id ?? null

  async function moderateQuestion(questionId, action) {
    if (!authToken) {
      addNotification('Login first.', 'error')
      return
    }

    if (!admin) {
      addNotification('Only session owner can moderate questions.', 'error')
      return
    }

    if (action === 'answer' && activeAnsweringQuestionId !== null && activeAnsweringQuestionId !== questionId) {
      addNotification('Finish the current in-progress question first.', 'error')
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
        addNotification('Question deleted.')
        await refreshQuestions()
      } else if (payload.banned) {
        addNotification('User banned. Questions deleted.')
        await refreshQuestions()
      } else if (payload.question) {
        if (action === 'answer') {
          addNotification('Question is now in progress.')
        } else if (action === 'finish_answering') {
          addNotification('Question moved to answered.')
        } else if (action === 'reject') {
          addNotification('Question rejected.')
        } else if (action === 'restore') {
          addNotification('Question restored.')
        } else if (action === 'reopen') {
          addNotification('Question reopened.')
        } else {
          addNotification('Question updated.')
        }
        await refreshQuestions()
      }
    } catch (error) {
      addNotification(error instanceof Error ? error.message : 'Moderation failed.', 'error')
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
          stream_link: settingsStreamLink.trim() || null,
          downvote_threshold: Math.max(1, Math.min(1000, Math.round(Number(settingsThreshold) || 5)))
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
    settingsThreshold = sessionData?.downvote_threshold ?? 5
    settingsStatus = ''
    showSessionSettings = true
  }

  async function loadBans() {
    loadingBans = true
    try {
      const payload = await apiRequest('/api/bans', { auth: true })
      bannedUsers = payload.bans ?? []
    } catch {
      // silently ignore
    } finally {
      loadingBans = false
    }
  }

  function toggleBannedUsers() {
    showBannedUsers = !showBannedUsers
    if (showBannedUsers) {
      void loadBans()
    }
  }

  async function unbanUser(userId) {
    unbanningUserId = userId
    try {
      await apiRequest(`/api/bans/${userId}`, {
        method: 'DELETE',
        auth: true
      })
      bannedUsers = bannedUsers.filter((b) => b.user_id !== userId)
    } catch (error) {
      homeMessage = error instanceof Error ? error.message : 'Failed to unban user.'
    } finally {
      unbanningUserId = null
    }
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

  function nowUnix() {
    return Math.floor(Date.now() / 1000)
  }

  function countLineBreaks(text) {
    return (text.replace(/\r\n/g, '\n').replace(/\r/g, '\n').match(/\n/g) || []).length
  }

  function formatDuration(seconds) {
    if (seconds < 60) return `${seconds}s`
    const min = Math.floor(seconds / 60)
    if (min < 60) return `${min}m`
    const hr = Math.floor(min / 60)
    const remMin = min % 60
    return remMin > 0 ? `${hr}h ${remMin}m` : `${hr}h`
  }

  function setTimecodeStartFromUnix(unixTime) {
    const d = new Date(unixTime * 1000)
    timecodeDay = d.getDate()
    timecodeMonth = d.getMonth() + 1
    timecodeYear = d.getFullYear()
    timecodeHour = d.getHours()
    timecodeMinute = d.getMinutes()
    timecodeSecond = d.getSeconds()
  }

  function formatTimecode(totalSeconds) {
    if (totalSeconds < 0) totalSeconds = 0
    const h = Math.floor(totalSeconds / 3600)
    const m = Math.floor((totalSeconds % 3600) / 60)
    const s = Math.floor(totalSeconds % 60)
    const pad = (n) => String(n).padStart(2, '0')
    if (h > 0) return `${h}:${pad(m)}:${pad(s)}`
    return `${m}:${pad(s)}`
  }

  async function loadTimecodeQuestions() {
    timecodeLoading = true
    try {
      const payload = await apiRequest(
        `/api/sessions/${encodeURIComponent(route.code)}/questions?sort=answered`,
        { auth: true }
      )
      timecodeQuestions = (payload.questions ?? [])
        .filter((q) => q.is_answered === 1 && q.answered_at)
        .sort((a, b) => a.answered_at - b.answered_at)
      timecodeQuestionsLoaded = true
    } catch {
      // silently ignore
    } finally {
      timecodeLoading = false
    }
  }

  async function copyTimecodes() {
    try {
      await navigator.clipboard.writeText(timecodeText)
      timecodeCopied = true
      setTimeout(() => { timecodeCopied = false }, 2000)
    } catch {
      // clipboard unavailable
    }
  }

  $: timecodeStreamStartUnix = Math.floor(
    new Date(timecodeYear, timecodeMonth - 1, timecodeDay, timecodeHour, timecodeMinute, timecodeSecond).getTime() / 1000
  )

  $: timecodeText = timecodeQuestions.length === 0
    ? ''
    : timecodeQuestions
        .map((q) => `${formatTimecode(q.answered_at - timecodeStreamStartUnix)} ${q.body}`)
        .join('\n')

  $: if (admin && sessionData?.is_active === 0 && !timecodeQuestionsLoaded) {
    setTimecodeStartFromUnix(sessionData.created_at)
    void loadTimecodeQuestions()
  }
</script>

<svelte:head>
  <title>{sessionData?.name ? `${sessionData.name} | qstream` : 'qstream'}</title>
</svelte:head>

<div class="app-shell">
  {#if route.name === 'home'}
    <!-- HOME PAGE -->
    <div class="app-body" style="display: grid; place-items: center; min-height: 100vh;">
      <div class="card card-centered">
        <span class="label-tag" style="display: flex; align-items: center; gap: 6px; justify-content: center;">
          <img src="/icon-64.webp" alt="" style="width: 20px; height: 20px;" />QStream
        </span>
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
            <p class="text-sm text-secondary" style="margin-bottom: 12px; display: flex; align-items: center; gap: 6px;"><Spinner size={13} />Loading sessions...</p>
          {:else if userSessions.length > 0}
            <div style="display: flex; flex-direction: column; gap: 8px; margin-bottom: 16px;">
              {#each userSessions as session}
                <div class="surface-row">
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
            <form on:submit={createSession} class="form-compact">
              <SessionDetailsFields
                prefix="create"
                bind:name={createName}
                bind:description={createDescription}
                bind:streamLink={createStreamLink}
              />
              <div class="form-actions form-actions-wrap">
                <button type="submit" class="btn btn-primary" disabled={createBusy}>
                  {createBusy ? 'Creating...' : 'Create'}
                </button>
                <button type="button" class="btn btn-ghost" on:click={() => { showCreateForm = false; createStatus = '' }}>
                  Cancel
                </button>
                {#if createStatus}
                  <span class="text-sm text-danger">{createStatus}</span>
                {/if}
              </div>
            </form>
          {:else}
            <button type="button" class="btn btn-primary" on:click={() => { showCreateForm = true; createStatus = '' }}>
              New session
            </button>
          {/if}

          <!-- Banned users section -->
          <div class="section-divider">
            <div style="display: flex; align-items: center; gap: 8px; margin-bottom: 4px;">
              <p class="text-sm" style="font-weight: 600; margin: 0;">Banned users</p>
              <button type="button" class="btn btn-ghost btn-sm" on:click={toggleBannedUsers}>
                {showBannedUsers ? 'Hide' : 'Details'}
              </button>
            </div>
            {#if showBannedUsers}
              {#if loadingBans}
                <p class="text-sm text-secondary" style="margin-top: 8px; display: flex; align-items: center; gap: 6px;"><Spinner size={13} />Loading...</p>
              {:else if bannedUsers.length === 0}
                <p class="text-sm text-secondary" style="margin-top: 8px;">No banned users.</p>
              {:else}
                <div style="display: flex; flex-direction: column; gap: 6px; margin-top: 8px;">
                  {#each bannedUsers as ban}
                    <div class="surface-card">
                      <div style="display: flex; align-items: center; justify-content: space-between; gap: 8px;">
                        <span style="font-weight: 600;">{ban.nickname}</span>
                        <button
                          type="button"
                          class="btn btn-secondary btn-sm"
                          disabled={unbanningUserId === ban.user_id}
                          on:click={() => unbanUser(ban.user_id)}
                        >Unban</button>
                      </div>
                      {#if ban.question_body}
                        <p class="text-sm text-secondary" style="margin: 4px 0 0; font-style: italic;">"{ban.question_body}"</p>
                      {/if}
                      <p class="text-sm text-secondary" style="margin: 2px 0 0;">
                        {ban.session_name ? `in ${ban.session_name} · ` : ''}banned {formatTime(ban.banned_at)}
                      </p>
                    </div>
                  {/each}
                </div>
              {/if}
            {/if}
          </div>
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
        <a class="app-logo" href="/" on:click|preventDefault={() => goto('/')}>
          <img src="/icon-64.webp" alt="" class="app-logo-icon" />qstream
        </a>

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
            <button type="button" class="btn btn-secondary btn-sm" on:click={() => (showSessionLogin = true)}>
              Log in
            </button>
          {/if}
        </div>
      </div>
    </header>

    <div class="app-body">
      <div style="margin-bottom: 20px;">
        <div style="display: flex; align-items: center; justify-content: space-between;">
          <h1 style="margin: 0;">{sessionData?.name || 'Session'}</h1>
          {#if admin}
            <button
              type="button"
              class="btn btn-secondary btn-sm"
              class:active={showSessionSettings}
              on:click={() => { if (showSessionSettings) { showSessionSettings = false } else { openSessionSettings() } }}
              title="Session settings"
              style="padding: 6px 8px; flex-shrink: 0;"
            >
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <circle cx="12" cy="12" r="3" stroke="currentColor" stroke-width="2"/>
                <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1Z" stroke="currentColor" stroke-width="2"/>
              </svg>
            </button>
          {/if}
        </div>
        {#if sessionData?.description}
          <p class="text-sm text-secondary" style="margin-top: 4px;">{sessionData.description}</p>
        {/if}
        {#if sessionData?.stream_link}
          <p class="text-sm" style="margin-top: 4px;">
            <a href={sessionData.stream_link} target="_blank" rel="noopener noreferrer">Watch stream</a>
          </p>
        {/if}
        {#if sessionData}
          <p class="text-sm text-secondary" style="margin-top: 4px;">
            {#if sessionData.is_active === 1}
              Active for {formatDuration(nowUnix() - sessionData.created_at)}
            {:else if sessionData.stopped_at}
              Was active for {formatDuration(sessionData.stopped_at - sessionData.created_at)}
            {/if}
          </p>
        {/if}
        {#if !currentUser}
          <p class="text-sm text-secondary" style="margin-top: 8px;">Log in to ask questions and vote.</p>
        {/if}
      </div>

      {#if sessionData && sessionData.is_active === 0}
        <div class="msg msg-info" style="margin-bottom: 16px;">
          This session has ended.
        </div>
      {/if}

      {#if viewerIsBanned}
        <div class="msg msg-error" style="margin-bottom: 16px;">
          You are banned.
        </div>
      {/if}

      {#if admin && showSessionSettings}
        <div class="card section-gap" style="margin-bottom: 16px;">
          <h3 style="margin: 0 0 12px;">Session settings</h3>
          <form on:submit={updateSessionSettings}>
            <div class="settings-form-body">
              <SessionDetailsFields
                prefix="settings"
                bind:name={settingsName}
                bind:description={settingsDescription}
                bind:streamLink={settingsStreamLink}
              />
              <div>
                <label for="settings-threshold" class="text-sm field-label">Hide questions below score</label>
                <div class="flex-row-gap-sm">
                  <input
                    id="settings-threshold"
                    type="number"
                    min="1"
                    max="1000"
                    bind:value={settingsThreshold}
                    style="width: 72px;"
                  />
                  <span class="text-sm text-secondary">Questions at −{settingsThreshold} or lower move to Bad tab</span>
                </div>
              </div>
              <div class="form-actions">
                <button type="submit" class="btn btn-primary btn-sm" disabled={settingsBusy}>
                  {settingsBusy ? 'Saving...' : 'Save'}
                </button>
                <button type="button" class="btn btn-ghost btn-sm" on:click={() => { showSessionSettings = false }}>
                  Cancel
                </button>
                {#if settingsStatus}
                  <span class="text-sm text-danger">{settingsStatus}</span>
                {/if}
              </div>

              {#if sessionData && sessionData.is_active === 1}
                <div class="section-divider-sm">
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

      {#if admin && sessionData && sessionData.is_active === 0}
        <div class="card section-gap" style="margin-bottom: 16px;">
          <h3 style="margin: 0 0 12px;">YouTube timecodes</h3>
          <div style="display: flex; flex-direction: column; gap: 10px;">
            <div>
              <p class="text-sm" style="margin: 0 0 6px;">Stream start time</p>
              <div style="display: flex; align-items: center; gap: 4px; flex-wrap: wrap;">
                <input type="number" min="1" max="31" bind:value={timecodeDay} style="width: 52px;" title="Day" />
                <span class="text-secondary">-</span>
                <input type="number" min="1" max="12" bind:value={timecodeMonth} style="width: 52px;" title="Month" />
                <span class="text-secondary">-</span>
                <input type="number" min="2000" max="2099" bind:value={timecodeYear} style="width: 76px;" title="Year" />
                <span style="margin-left: 6px;"></span>
                <input type="number" min="0" max="23" bind:value={timecodeHour} style="width: 52px;" title="Hour (0–23)" />
                <span class="text-secondary">:</span>
                <input type="number" min="0" max="59" bind:value={timecodeMinute} style="width: 52px;" title="Minute" />
                <span class="text-secondary">:</span>
                <input type="number" min="0" max="59" bind:value={timecodeSecond} style="width: 52px;" title="Second" />
                <button type="button" class="btn btn-ghost btn-sm" style="margin-left: 6px;" on:click={() => setTimecodeStartFromUnix(sessionData.created_at)}>Reset</button>
              </div>
              <p class="text-sm text-secondary" style="margin: 4px 0 0;">DD - MM - YYYY &nbsp; HH : MM : SS</p>
            </div>
            {#if timecodeLoading}
              <p class="text-sm text-secondary" style="display: flex; align-items: center; gap: 6px;"><Spinner size={13} />Loading questions...</p>
            {:else if timecodeText}
              <div>
                <label for="timecode-output" class="text-sm" style="display: block; margin-bottom: 4px;">Timecodes</label>
                <textarea
                  id="timecode-output"
                  readonly
                  rows={Math.min(Math.max(timecodeQuestions.length, 2), 12)}
                  style="width: 100%; box-sizing: border-box; font-family: monospace; font-size: 13px; resize: vertical;"
                  value={timecodeText}
                ></textarea>
              </div>
              <div>
                <button type="button" class="btn btn-secondary btn-sm" on:click={copyTimecodes}>
                  {timecodeCopied ? 'Copied!' : 'Copy'}
                </button>
              </div>
            {:else if timecodeQuestionsLoaded}
              <p class="text-sm text-secondary">No answered questions.</p>
            {/if}
          </div>
        </div>
      {/if}

      {#if !currentUser && showSessionLogin}
        <!-- svelte-ignore a11y-click-events-have-key-events a11y-no-static-element-interactions -->
        <div class="modal-backdrop" on:click={() => (showSessionLogin = false)}>
          <div class="modal-card" on:click|stopPropagation role="dialog" aria-modal="true" tabindex="-1">
            <button class="modal-close" type="button" aria-label="Close" on:click={() => (showSessionLogin = false)}>✕</button>
            <LoginPanel
              {apiBase}
              title="Log in to interact"
              subtitle="Continue with Google to ask questions and vote."
              submitLabel="Continue with Google"
              returnTo={`/s/${route.code}`}
            />
            <p class="text-sm text-secondary" style="margin-top: 12px;">Google login only requests your name — not your email address.</p>
          </div>
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
              <button type="submit" class="btn btn-primary btn-sm" disabled={questionBusy || questionCooldownRemaining > 0}>
                {#if questionBusy}
                  Sending...
                {:else if questionCooldownRemaining > 0}
                  <svg class="ask-cooldown" width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
                    <circle class="ask-cooldown-track" cx="6" cy="6" r="4.5" />
                    <circle style="stroke-dashoffset: {28.27 * (1 - questionCooldownRemaining / 60)}; transition: stroke-dashoffset 0.5s linear;" cx="6" cy="6" r="4.5" />
                  </svg><span style="font-variant-numeric: tabular-nums;">{questionCooldownRemaining}s</span>
                {:else}
                  Ask
                {/if}
              </button>
            </div>
          </form>
        </div>
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
          >Done</button>
          <button
            type="button"
            class="tab"
            class:active={sessionSort === 'downvoted'}
            on:click={() => changeSort('downvoted')}
          >Bad</button>
          {#if admin}
            <button
              type="button"
              class="tab"
              class:active={sessionSort === 'deleted'}
              on:click={() => changeSort('deleted')}
            >Deleted</button>
          {/if}
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

        <button type="button" class="btn btn-secondary btn-sm refresh-btn" on:click={() => refreshQuestions()} disabled={loadingQuestions} title="Refresh">
          <span class="refresh-label" class:invisible={loadingQuestions}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <path d="M4 12a8 8 0 0 1 14.93-4H15" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>
              <path d="M20 12a8 8 0 0 1-14.93 4H9" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>
            </svg>
          </span>
          {#if loadingQuestions}
            <span class="refresh-spinner"><Spinner size={13} /></span>
          {/if}
          {#if pendingNewQuestions > 0}
            <span class="refresh-badge">{pendingNewQuestions}</span>
          {/if}
        </button>

      </div>

      <!-- Question list -->
      {#if loadedSessionSort === 'downvoted' && sessionData}
        <div class="msg msg-info" style="margin-bottom: 12px; text-align: center;">
          Questions with a score of −{sessionData.downvote_threshold ?? 5} or lower are hidden from other tabs.
        </div>
      {/if}
      {#if loadedSessionSort === 'deleted' && sessionData}
        <div class="msg msg-info" style="margin-bottom: 12px; text-align: center;">
          Only the session owner can see deleted questions.
        </div>
      {/if}
      <div class="q-list">
        {#if visibleQuestions.length === 0 && !loadingQuestions}
          <div class="empty-state">
            <div class="empty-state-icon">?</div>
            <p>
              {#if hideInteracted && currentUser}
                All questions filtered.
              {:else if loadedSessionSort === 'deleted'}
                No deleted questions.
              {:else}
                No questions yet.
              {/if}
            </p>
          </div>
        {/if}

        {#each visibleQuestions as item}
          <article class="q-card" class:answering={item.is_answering === 1} class:answered={item.is_answered === 1} class:rejected={item.is_rejected === 1} class:new-highlight={item.id === newQuestionId}>
            <div class="q-vote-col">
              <button
                type="button"
                class="q-vote-btn"
                class:upvoted={localVotes[item.id] === 1}
                on:click={() => vote(item.id, localVotes[item.id] === 1 ? 0 : 1)}
                disabled={!viewerCanInteract || voteBusy.has(item.id) || item.is_answered === 1 || item.is_answering === 1 || item.is_rejected === 1 || item.is_deleted === 1}
                title="Upvote"
              >&#9650;</button>

              <span class="q-score">{item.score}</span>

              <button
                type="button"
                class="q-vote-btn"
                class:downvoted={localVotes[item.id] === -1}
                on:click={() => vote(item.id, localVotes[item.id] === -1 ? 0 : -1)}
                disabled={!viewerCanInteract || voteBusy.has(item.id) || item.is_answered === 1 || item.is_answering === 1 || item.is_rejected === 1 || item.is_deleted === 1}
                title="Downvote"
              >&#9660;</button>
            </div>

            <div class="q-body-col">
              {#if item.is_answering === 1}
                <span class="badge badge-answering">Answering</span>
              {:else if item.is_answered === 1}
                <span class="badge badge-answered">Answered</span>
              {:else if item.is_rejected === 1}
                <span class="badge badge-rejected">Rejected</span>
              {:else if item.is_deleted === 1}
                <span class="badge badge-rejected">Deleted</span>
              {/if}

              <p class="q-text">{item.body}</p>

              <div class="q-meta">
                <span>{item.author_nickname}</span>
                <span class="q-meta-sep">&middot;</span>
                <span>{formatTime(item.created_at)}</span>
                <span class="q-meta-sep">&middot;</span>
                <span>{item.votes_count} vote{item.votes_count === 1 ? '' : 's'}</span>
                {#if item.is_answering === 1 && item.answering_started_at}
                  <span class="q-meta-sep">&middot;</span>
                  <span>answering for {formatDuration(nowUnix() - item.answering_started_at)}</span>
                {:else if item.is_answered === 1 && item.answered_at && item.answering_started_at}
                  <span class="q-meta-sep">&middot;</span>
                  <span>answered in {formatDuration(item.answered_at - item.answering_started_at)}</span>
                {/if}
              </div>

              {#if admin}
                <div class="q-actions" style="display: flex; align-items: center; gap: 6px;">
                  {#if item.is_deleted === 1}
                    <button
                      type="button"
                      class="btn btn-secondary btn-sm"
                      on:click={() => moderateQuestion(item.id, 'restore')}
                      disabled={moderateBusy.has(item.id)}
                    >Restore</button>
                    <div style="margin-left: auto; display: flex; gap: 6px;">
                      <ConfirmButton
                        class="btn btn-danger btn-sm"
                        label={item.author_is_banned === 1 ? 'Banned' : 'Ban'}
                        confirmLabel="Confirm ban"
                        disabled={moderateBusy.has(item.id) || item.author_is_banned === 1}
                        on:confirm={() => moderateQuestion(item.id, 'ban')}
                      />
                    </div>
                  {:else}
                    {#if sessionData?.is_active === 1}
                      {#if item.is_answered === 0}
                        <button
                          type="button"
                          class="btn btn-secondary btn-sm"
                          on:click={() => moderateQuestion(item.id, item.is_answering === 1 ? 'finish_answering' : 'answer')}
                          disabled={moderateBusy.has(item.id) || (item.is_answering === 0 && activeAnsweringQuestionId !== null)}
                          title={item.is_answering === 0 && activeAnsweringQuestionId !== null ? 'Finish current in-progress question first' : undefined}
                        >{item.is_answering === 1 ? 'Done' : 'Answer'}</button>
                      {/if}

                      {#if item.is_answering === 1 || item.is_answered === 1}
                        <button
                          type="button"
                          class="btn btn-secondary btn-sm"
                          on:click={() => moderateQuestion(item.id, 'reopen')}
                          disabled={moderateBusy.has(item.id)}
                        >Undo</button>
                      {/if}

                      {#if item.is_answered === 0 && item.is_rejected === 0}
                        <button
                          type="button"
                          class="btn btn-secondary btn-sm"
                          on:click={() => moderateQuestion(item.id, 'reject')}
                          disabled={moderateBusy.has(item.id)}
                        >Reject</button>
                      {/if}
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
                        label={item.author_is_banned === 1 ? 'Banned' : 'Ban'}
                        confirmLabel="Confirm ban"
                        disabled={moderateBusy.has(item.id) || item.author_is_banned === 1}
                        on:confirm={() => moderateQuestion(item.id, 'ban')}
                      />
                    </div>
                  {/if}
                </div>
              {/if}
            </div>
          </article>
        {/each}
      </div>
    </div>
  {/if}
</div>

<Notifications history={notifHistory} toasts={activeToasts} />

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.45);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 200;
    padding: 16px;
  }

  .modal-card {
    background: var(--bg-card, #fff);
    border-radius: 12px;
    padding: 28px 24px 24px;
    width: 100%;
    max-width: 360px;
    position: relative;
    box-shadow: 0 24px 64px rgba(0, 0, 0, 0.25);
  }

  .modal-close {
    position: absolute;
    top: 10px;
    right: 12px;
    background: none;
    border: none;
    cursor: pointer;
    font-size: 16px;
    line-height: 1;
    padding: 4px 6px;
    color: var(--ink-secondary, #888);
    border-radius: 4px;
  }

  .modal-close:hover {
    background: var(--bg-hover, #f0f0f0);
  }

  .q-card.new-highlight {
    animation: new-question-fade 3s ease-out forwards;
  }

  @keyframes new-question-fade {
    from { box-shadow: 0 0 0 2px rgba(5, 150, 105, 0.8), 0 0 16px rgba(5, 150, 105, 0.3); }
    to   { box-shadow: 0 0 0 2px rgba(5, 150, 105, 0),   0 0 16px rgba(5, 150, 105, 0); }
  }

  .ask-cooldown {
    display: inline-block;
    margin-right: 4px;
    vertical-align: -2px;
    flex-shrink: 0;
  }

  .ask-cooldown circle {
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
  }

  .ask-cooldown-track {
    opacity: 0.35;
  }

  .ask-cooldown circle:not(.ask-cooldown-track) {
    stroke-dasharray: 28.27;
    transform: scale(-1, 1) rotate(-90deg);
    transform-origin: 6px 6px;
  }
</style>
