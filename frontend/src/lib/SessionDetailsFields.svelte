<script>
  export let prefix = 'session'
  export let name = ''
  export let description = ''
  export let streamLink = ''
</script>

<div class="session-details-fields">
  <div>
    <label for={`${prefix}-name`} class="text-sm field-label">Name <span class="text-danger">*</span></label>
    <input
      id={`${prefix}-name`}
      type="text"
      maxlength="100"
      bind:value={name}
      placeholder="My Stream Q&A"
    />
  </div>

  <div>
    <label for={`${prefix}-description`} class="text-sm field-label">Description</label>
    <textarea
      id={`${prefix}-description`}
      maxlength="500"
      bind:value={description}
      placeholder="Short description shown to viewers"
      rows="2"
    ></textarea>
  </div>

  <div>
    <label for={`${prefix}-stream-link`} class="text-sm field-label">Stream link</label>
    <input
      id={`${prefix}-stream-link`}
      type="url"
      maxlength="500"
      bind:value={streamLink}
      placeholder="https://twitch.tv/yourname"
    />
  </div>
</div>

<style>
  .session-details-fields {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
</style>
