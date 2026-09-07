// Secrets Management Dashboard
// Phase 5: Web UI for viewing and managing encrypted secrets

const SecretsUI = {
  async init() {
    this.container = document.createElement('div');
    this.container.id = 'secrets-dashboard';
    this.container.className = 'secrets-container';
    this.setupStyles();
    this.render();
  },

  setupStyles() {
    const style = document.createElement('style');
    style.textContent = `
      .secrets-container {
        padding: 20px;
        max-width: 1200px;
        margin: 0 auto;
      }
      .secrets-header {
        display: flex;
        justify-content: space-between;
        align-items: center;
        margin-bottom: 30px;
        border-bottom: 2px solid var(--border-color);
        padding-bottom: 15px;
      }
      .secrets-header h1 {
        margin: 0;
        font-size: 28px;
      }
      .secrets-search {
        display: flex;
        gap: 10px;
        margin-bottom: 20px;
      }
      .secrets-search input {
        flex: 1;
        padding: 10px;
        border: 1px solid var(--border-color);
        border-radius: 4px;
        font-family: monospace;
      }
      .secrets-list {
        display: grid;
        grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
        gap: 15px;
      }
      .secret-card {
        border: 1px solid var(--border-color);
        border-radius: 8px;
        padding: 15px;
        background: var(--card-bg);
      }
      .secret-card h3 {
        margin: 0 0 10px 0;
        font-family: monospace;
        font-size: 14px;
        word-break: break-all;
      }
      .secret-card .value {
        font-family: monospace;
        padding: 8px;
        background: var(--code-bg);
        border-radius: 4px;
        margin: 10px 0;
        font-size: 12px;
        max-height: 100px;
        overflow-y: auto;
      }
      .secret-card .hidden {
        color: var(--text-muted);
      }
      .secret-metadata {
        background: var(--code-bg);
        border-radius: 4px;
        padding: 10px;
        margin: 10px 0;
        font-size: 13px;
      }
      .metadata-item {
        margin: 6px 0;
        padding: 4px 0;
        border-bottom: 1px solid var(--border-color);
      }
      .metadata-item:last-child {
        border-bottom: none;
      }
      .metadata-item strong {
        color: var(--primary-color);
      }
      .rotation-warning {
        color: #ff6b6b;
      }
      .warning-badge {
        display: inline-block;
        background: #ff6b6b;
        color: white;
        padding: 3px 8px;
        border-radius: 3px;
        font-size: 12px;
        font-weight: bold;
      }
      .used-by {
        font-size: 12px;
        color: var(--text-muted);
        margin: 8px 0;
        padding: 8px;
        background: var(--code-bg);
        border-radius: 4px;
        border-left: 3px solid var(--primary-color);
      }
      .used-by strong {
        color: var(--primary-color);
      }
      .secret-controls {
        display: flex;
        gap: 8px;
        margin-top: 10px;
      }
      .secret-controls button {
        flex: 1;
        padding: 8px;
        border: 1px solid var(--border-color);
        border-radius: 4px;
        background: var(--button-bg);
        cursor: pointer;
        font-size: 12px;
      }
      .secret-controls button:hover {
        background: var(--button-hover-bg);
      }
      .modal {
        position: fixed;
        top: 0;
        left: 0;
        right: 0;
        bottom: 0;
        background: rgba(0,0,0,0.5);
        display: flex;
        align-items: center;
        justify-content: center;
        z-index: 1000;
      }
      .modal-content {
        background: var(--card-bg);
        padding: 30px;
        border-radius: 8px;
        max-width: 500px;
        width: 90%;
      }
      .modal-content h2 {
        margin-top: 0;
      }
      .form-group {
        margin-bottom: 15px;
      }
      .form-group label {
        display: block;
        margin-bottom: 5px;
        font-weight: bold;
        font-size: 14px;
      }
      .form-group input, .form-group textarea {
        width: 100%;
        padding: 10px;
        border: 1px solid var(--border-color);
        border-radius: 4px;
        font-family: monospace;
      }
      .form-actions {
        display: flex;
        gap: 10px;
        justify-content: flex-end;
        margin-top: 20px;
      }
      .form-actions button {
        padding: 10px 20px;
        border: 1px solid var(--border-color);
        border-radius: 4px;
        background: var(--button-bg);
        cursor: pointer;
      }
      .form-actions button.primary {
        background: var(--primary-color);
        color: white;
        border-color: var(--primary-color);
      }
      .status-message {
        padding: 15px;
        margin-bottom: 20px;
        border-radius: 4px;
        display: none;
      }
      .status-message.success {
        display: block;
        background: var(--success-bg);
        color: var(--success-text);
      }
      .status-message.error {
        display: block;
        background: var(--error-bg);
        color: var(--error-text);
      }
      @media (max-width: 600px) {
        .secrets-list {
          grid-template-columns: 1fr;
        }
        .secrets-header {
          flex-direction: column;
          align-items: flex-start;
        }
      }
    `;
    document.head.appendChild(style);
  },

  async render() {
    this.container.innerHTML = `
      <div class="secrets-header">
        <h1>🔐 Secrets Manager</h1>
        <button id="add-secret-btn" style="padding: 10px 20px; cursor: pointer;">
          + New Secret
        </button>
      </div>
      <div class="secrets-search">
        <input 
          type="text" 
          id="search-secrets" 
          placeholder="Search secrets by path (e.g., oauth.google)..."
        />
      </div>
      <div id="status-message" class="status-message"></div>
      <div id="secrets-list" class="secrets-list">
        <p>Loading secrets...</p>
      </div>
    `;

    document.body.appendChild(this.container);
    
    // Load secrets list
    await this.loadSecrets();
    
    // Event listeners
    document.getElementById('add-secret-btn').addEventListener('click', 
      () => this.showAddModal());
    document.getElementById('search-secrets').addEventListener('input', 
      (e) => this.filterSecrets(e.target.value));
  },

  async loadSecrets() {
    try {
      // Fetch manifest with metadata (Phase 4 enhancement)
      // Note: Works even without service worker (iOS private mode fallback)
      // Use relative URL so it works on any host/port (localhost, LAN IP, iOS)
      const response = await fetch('/api/secrets/manifest', {
        headers: {
          'X-Requested-With': 'XMLHttpRequest',
          'Cache-Control': 'no-cache'  // Force fresh fetch in private mode
        },
        cache: 'no-store'  // Prevent caching issues on iOS
      });

      if (!response.ok) throw new Error(`HTTP ${response.status}`);

      const data = await response.json();
      this.secretsWithMetadata = data.secrets || [];
      this.displaySecrets(this.secretsWithMetadata);
    } catch (error) {
      console.error('Failed to load secrets:', error);
      // Provide helpful error with debugging info
      const msg = error.message || 'Unknown error';
      const details = navigator.onLine ? '' : ' (offline)';
      document.getElementById('secrets-list').innerHTML =
        `<p style="color: red;">Error loading secrets: ${msg}${details}</p>
         <p style="color: #999; font-size: 12px;">Try: Refresh, non-private mode, or check network</p>`;
    }
  },

  displaySecrets(secrets) {
    const list = document.getElementById('secrets-list');

    if (!secrets.length) {
      list.innerHTML = '<p>No secrets found.</p>';
      return;
    }

    list.innerHTML = secrets.map(secret => `
      <div class="secret-card" data-path="${secret.path}">
        <h3>${secret.path}</h3>
        <div class="secret-metadata">
          ${secret.service ? `<div class="metadata-item"><strong>Service:</strong> ${secret.service}</div>` : ''}
          ${secret.purpose ? `<div class="metadata-item"><strong>Purpose:</strong> ${secret.purpose}</div>` : ''}
          ${secret.owner ? `<div class="metadata-item"><strong>Owner:</strong> ${secret.owner}</div>` : ''}
          ${secret.rotation_days ? `<div class="metadata-item"><strong>Rotation:</strong> ${secret.rotation_days} days</div>` : ''}
          ${secret.needs_rotation ? `<div class="metadata-item rotation-warning"><span class="warning-badge">⚠️ Needs Rotation</span></div>` : ''}
        </div>
        ${secret.used_by && secret.used_by.length > 0 ? `
          <div class="used-by">
            <strong>Used by:</strong> ${secret.used_by.join(', ')}
          </div>
        ` : ''}
        <div class="secret-controls">
          <button onclick="SecretsUI.showViewModal('${secret.path}')">View</button>
          <button onclick="SecretsUI.showEditModal('${secret.path}')">Edit</button>
          <button onclick="SecretsUI.copyToClipboard('${secret.path}')">Copy</button>
        </div>
      </div>
    `).join('');
  },

  filterSecrets(query) {
    // Search in path, service, purpose, and owner
    const filtered = this.secretsWithMetadata.filter(secret =>
      secret.path.includes(query) ||
      (secret.service && secret.service.includes(query)) ||
      (secret.purpose && secret.purpose.includes(query)) ||
      (secret.owner && secret.owner.includes(query))
    );
    this.displaySecrets(filtered);
  },

  async showViewModal(path) {
    try {
      const response = await fetch(`https://localhost:8823/api/secrets/${path}`, {
        headers: { 'X-Requested-With': 'XMLHttpRequest' }
      });
      
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      
      const data = await response.json();
      const value = data.value || '(empty)';
      
      const modal = this.createModal(
        `View Secret: ${path}`,
        `<div class="form-group">
          <label>Value:</label>
          <textarea readonly style="height: 150px;">${value}</textarea>
        </div>`,
        [{ label: 'Close', class: 'primary', action: () => this.closeModal() }]
      );
      
      this.showModal(modal);
    } catch (error) {
      this.showStatus(`Error loading secret: ${error.message}`, 'error');
    }
  },

  async showEditModal(path) {
    const modal = this.createModal(
      `Edit Secret: ${path}`,
      `<div class="form-group">
        <label>New Value:</label>
        <textarea id="edit-value" style="height: 150px;"></textarea>
      </div>`,
      [
        { label: 'Cancel', action: () => this.closeModal() },
        { 
          label: 'Save', 
          class: 'primary', 
          action: async () => {
            await this.saveSecret(path);
            this.closeModal();
            await this.loadSecrets();
          }
        }
      ]
    );
    
    this.showModal(modal);
  },

  async saveSecret(path) {
    const value = document.getElementById('edit-value').value;
    
    try {
      const response = await fetch(`https://localhost:8823/api/secrets/${path}`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'X-Requested-With': 'XMLHttpRequest'
        },
        body: JSON.stringify({ value })
      });
      
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      
      this.showStatus(`✓ Secret updated: ${path}`, 'success');
    } catch (error) {
      this.showStatus(`Error saving secret: ${error.message}`, 'error');
    }
  },

  async showAddModal() {
    const modal = this.createModal(
      'Add New Secret',
      `<div class="form-group">
        <label>Path (e.g., external_services.api_key):</label>
        <input id="new-path" type="text" placeholder="dot.separated.path" />
      </div>
      <div class="form-group">
        <label>Value:</label>
        <textarea id="new-value" style="height: 150px;"></textarea>
      </div>`,
      [
        { label: 'Cancel', action: () => this.closeModal() },
        { 
          label: 'Create', 
          class: 'primary', 
          action: async () => {
            const path = document.getElementById('new-path').value;
            const value = document.getElementById('new-value').value;
            if (path && value) {
              await this.saveSecret(path);
              this.closeModal();
              await this.loadSecrets();
            }
          }
        }
      ]
    );
    
    this.showModal(modal);
  },

  async copyToClipboard(path) {
    try {
      const response = await fetch(`https://localhost:8823/api/secrets/${path}`, {
        headers: { 'X-Requested-With': 'XMLHttpRequest' }
      });
      
      if (response.ok) {
        const data = await response.json();
        await navigator.clipboard.writeText(data.value);
        this.showStatus(`✓ Copied to clipboard`, 'success');
      }
    } catch (error) {
      this.showStatus(`Error copying: ${error.message}`, 'error');
    }
  },

  createModal(title, content, buttons) {
    const modal = document.createElement('div');
    modal.className = 'modal';
    modal.innerHTML = `
      <div class="modal-content">
        <h2>${title}</h2>
        ${content}
        <div class="form-actions">
          ${buttons.map(btn => `
            <button class="${btn.class || ''}">${btn.label}</button>
          `).join('')}
        </div>
      </div>
    `;
    
    const btns = modal.querySelectorAll('button');
    btns.forEach((btn, i) => {
      btn.addEventListener('click', buttons[i].action);
    });
    
    modal.addEventListener('click', (e) => {
      if (e.target === modal) this.closeModal();
    });
    
    return modal;
  },

  showModal(modal) {
    document.body.appendChild(modal);
  },

  closeModal() {
    const modal = document.querySelector('.modal');
    if (modal) modal.remove();
  },

  showStatus(message, type) {
    const el = document.getElementById('status-message');
    el.textContent = message;
    el.className = `status-message ${type}`;
    setTimeout(() => el.className = 'status-message', 3000);
  }
};

// Initialize when document loads
if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', () => SecretsUI.init());
} else {
  SecretsUI.init();
}
