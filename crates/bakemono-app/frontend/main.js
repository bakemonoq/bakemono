const { invoke } = window.__TAURI__.core
const { listen } = window.__TAURI__.event

const $ = (id) => document.getElementById(id)
const log = (line) => {
  const el = $('log')
  el.textContent += line + '\n'
  el.scrollTop = el.scrollHeight
}

function render(p) {
  switch (p.stage) {
    case 'scraping': return `scraping ${p.creator} -> ${p.dest}`
    case 'scrape_post': return `post #${p.posts}: ${p.file}`
    case 'scraped': return `scraped ${p.files} file(s) across ${p.posts} post(s)`
    case 'pairs': return `${p.count} media+sidecar pair(s)`
    case 'seeder_ready': return 'seeder ready'
    case 'manifest': return `[${p.index}/${p.total}] ${p.file} ${p.hash.slice(0, 16)} (${p.size} bytes)`
    case 'seeded': return `seeded ${p.file}`
    case 'skipped': return `skip ${p.file}: ${p.reason}`
    case 'publishing': return `publishing ${p.count} event(s) to ${p.relays.join(', ')}`
    case 'published': return `published ${p.event_ids.length} event(s)`
    case 'cancelled': return 'cancelled'
    case 'done': return `done, ${p.manifests} manifest(s)`
    case 'failed': return `failed: ${p.error}`
    default: return JSON.stringify(p)
  }
}

listen('progress', (e) => log('  ' + render(e.payload)))

function setRunning(running) {
  $('stop').disabled = !running
  $('scrape').disabled = running
  $('runState').textContent = running ? 'running' : ''
}

async function withJob(label, fn) {
  setRunning(true)
  log(`> ${label}`)
  try {
    const summary = await fn()
    log(`OK: ${summary.event_ids.length} event(s) published`)
  } catch (err) {
    log('ERROR: ' + err)
  } finally {
    setRunning(false)
    refreshStats()
  }
}

function fmtBytes(n) {
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let i = 0
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++ }
  return `${n.toFixed(i ? 1 : 0)} ${units[i]}`
}

async function refreshStats() {
  try {
    const s = await invoke('sharing_stats')
    $('mFiles').textContent = s.files
    $('mBytes').textContent = fmtBytes(s.total_bytes)
    $('mPosts').textContent = s.posts
    $('mCreators').textContent = s.creators
  } catch {}
}

async function refreshIdentity() {
  try {
    $('npub').textContent = await invoke('identity_npub')
  } catch (err) {
    $('npub').textContent = 'error: ' + err
  }
}

async function refreshConfig() {
  const cfg = await invoke('get_config')
  $('relays').value = cfg.relays.join('\n')
  $('trackers').value = cfg.trackers.join('\n')
  $('stun').value = cfg.stun.join('\n')
  $('maxUp').value = cfg.max_up_mbit
  $('maxDown').value = cfg.max_down_mbit
  $('stopOnExit').checked = !!cfg.stop_daemon_on_exit
}

const lines = (id) => $(id).value.split('\n').map((s) => s.trim()).filter(Boolean)

function pathRow(label, path) {
  return `<div class="kv"><span class="lbl2">${label}</span><code>${path}</code>` +
    `<button class="open" data-path="${path}">Open</button></div>`
}

async function refreshPaths() {
  const p = await invoke('app_paths')
  $('paths').innerHTML = pathRow('scrapes', p.scrape_dir) + pathRow('logs', p.log_dir)
}

$('paths').addEventListener('click', (e) => {
  const btn = e.target.closest('button.open')
  if (!btn) return
  invoke('open_path', { path: btn.dataset.path }).catch((err) => log('open failed: ' + err))
})

async function refreshDaemon() {
  try {
    const s = await invoke('daemon_status')
    $('ddot').className = 'dot up'
    $('dstatus').textContent = s.running ? 'running a job' : (s.seeding ? 'seeding' : 'idle')
  } catch {
    $('ddot').className = 'dot down'
    $('dstatus').textContent = 'stopped'
  }
}

$('drestart').onclick = async () => {
  $('ddot').className = 'dot'
  $('dstatus').textContent = 'restarting...'
  try { await invoke('restart_daemon') } catch (e) { log('restart failed: ' + e) }
  refreshDaemon()
}

$('dstop').onclick = async () => {
  try { await invoke('stop_daemon') } catch (e) { log('stop failed: ' + e) }
  refreshDaemon()
}

$('stopOnExit').onchange = async () => {
  try { await invoke('set_stop_on_exit', { value: $('stopOnExit').checked }) } catch (e) { log('failed: ' + e) }
}

// identity/relay changes only take effect once the daemon reloads them
async function applyToDaemon() {
  try { await invoke('restart_daemon') } catch (e) { log('daemon restart failed: ' + e) }
  refreshDaemon()
}

$('gen').onclick = async () => {
  if (!confirm('Replace the current identity with a new keypair?')) return
  $('npub').textContent = await invoke('generate_identity')
  log('generated new identity, restarting daemon')
  applyToDaemon()
}

$('imp').onclick = async () => {
  const nsec = prompt('Paste your nsec')
  if (!nsec) return
  try {
    $('npub').textContent = await invoke('import_identity', { nsec })
    log('imported identity, restarting daemon')
    applyToDaemon()
  } catch (err) {
    alert('import failed: ' + err)
  }
}

$('exp').onclick = async () => {
  const nsec = await invoke('export_nsec')
  prompt('Your nsec (keep it secret):', nsec)
}

$('saveSettings').onclick = async () => {
  const relays = lines('relays')
  const trackers = lines('trackers')
  const stun = lines('stun')
  const maxUpMbit = Number($('maxUp').value) || 0
  const maxDownMbit = Number($('maxDown').value) || 0
  await invoke('save_settings', { relays, trackers, stun, maxUpMbit, maxDownMbit })
  log(`saved settings; restarting daemon`)
  applyToDaemon()
}

$('stop').onclick = async () => {
  await invoke('cancel_job')
  log('stop requested')
}

$('patreonLogin').onclick = async () => {
  try {
    await invoke('open_patreon_login')
    log('opened Patreon login - sign in and the session saves automatically')
  } catch (err) {
    log('could not open login: ' + err)
  }
}

function markSignedIn(text) {
  $('cookieDot').className = 'dot up'
  $('cookieStatus').textContent = text
  $('patreonLogin').textContent = 'Re-login'
}

// the rust side captures cookies once the popup reaches a logged-in page, then closes it
listen('patreon-captured', (e) => {
  const { count, path } = e.payload
  $('cookies').value = path
  $('browser').value = ''
  markSignedIn(`signed in (${count} cookies)`)
  log(`Patreon session saved (${count} cookies)`)
})

async function refreshCookies() {
  const path = await invoke('saved_patreon_cookies')
  if (path) {
    if (!$('cookies').value) $('cookies').value = path
    markSignedIn('signed in (saved session)')
  }
}

$('scrape').onclick = () => {
  const creator = $('creator').value.trim()
  if (!creator) return alert('enter a creator')
  const limitRaw = $('limit').value.trim()
  const limit = limitRaw ? Number(limitRaw) : null
  const cookies = $('cookies').value.trim() || null
  const browser = $('browser').value || null
  withJob(`scrape ${creator}${limit ? ' (max ' + limit + ' posts)' : ''}`,
    () => invoke('start_scrape', { creator, limit, cookies, browser }))
}

refreshIdentity()
refreshConfig()
refreshPaths()
refreshStats()
refreshCookies()
// start the daemon if it isn't already up, then keep the status dot live
invoke('start_daemon').catch((e) => log('daemon start failed: ' + e)).finally(refreshDaemon)
setInterval(refreshDaemon, 3000)
