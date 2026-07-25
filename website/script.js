// Interactive Terminal and Features Script for octane brutalist website

document.addEventListener('DOMContentLoaded', () => {
  // 1. Tabbed Code Box Switching
  const tabBtns = document.querySelectorAll('.tab-btn');
  tabBtns.forEach(btn => {
    btn.addEventListener('click', () => {
      tabBtns.forEach(b => b.classList.remove('active'));
      document.querySelectorAll('.tab-content').forEach(c => c.classList.remove('active'));

      btn.classList.add('active');
      const tabId = btn.getAttribute('data-tab');
      const targetContent = document.getElementById(`tab-${tabId}`);
      if (targetContent) {
        targetContent.classList.add('active');
      }
    });
  });

  // 2. Copy Code Snippet Functionality
  const copyBtns = document.querySelectorAll('.copy-btn');
  copyBtns.forEach(btn => {
    btn.addEventListener('click', () => {
      const textToCopy = btn.getAttribute('data-copy');
      if (textToCopy) {
        navigator.clipboard.writeText(textToCopy).then(() => {
          const originalText = btn.textContent;
          btn.textContent = '[COPIED!]';
          btn.style.backgroundColor = '#ffffff';
          btn.style.color = '#000000';
          setTimeout(() => {
            btn.textContent = originalText;
            btn.style.backgroundColor = '';
            btn.style.color = '';
          }, 2000);
        }).catch(err => {
          console.error('Copy failed:', err);
        });
      }
    });
  });

  // 3. Crate Architecture Interactive Inspector
  const cratesData = {
    sandbox: {
      title: 'octane-sandbox',
      desc: 'Enforces real OS containment utilizing native kernel features (macOS Seatbelt, Linux Landlock, Windows AppContainer). Ensures processes spawning tool tasks cannot touch files outside declared workspace roots or write to .git/ hooks.',
      owns: 'Seatbelt / Landlock / AppContainer containment',
      doesNotOwn: 'User consent decisions (handled by octane-permission)'
    },
    permission: {
      title: 'octane-permission',
      desc: 'Action policy engine deciding whether actions are allowed, denied, or require user approval before execution.',
      owns: 'action(target) policy → allow / ask / deny; modes',
      doesNotOwn: 'OS kernel containment enforcement'
    },
    tools: {
      title: 'octane-tools',
      desc: 'Contains built-in safe execution tools (read, write, edit, bash, glob, grep, list, task) with deterministic mtime sorting and BTreeMap schema serialization.',
      owns: 'Tool trait, ToolRegistry, the built-in tools',
      doesNotOwn: 'Whether a tool call is permitted'
    },
    provider: {
      title: 'octane-provider',
      desc: 'Normalizes LLM stream events and supports OpenAI, Anthropic, Google Gemini, and custom corporate sigv4/token endpoints.',
      owns: 'LanguageModel trait, normalized StreamEvent, ProviderTransform, pricing',
      doesNotOwn: 'Prompt assembly, tool execution'
    },
    protocol: {
      title: 'octane-protocol',
      desc: 'The shared vocabulary across all crates. Standardizes thread models, turns, items, messages, and event wire structures.',
      owns: 'Thread / Turn / Item / Message / Part / Event vocabulary',
      doesNotOwn: 'Any executable behavior'
    },
    core: {
      title: 'octane-core',
      desc: 'Coordinates the agent ReAct loop, stop conditions, and prompt assembly. Pure coordination with zero domain logic coupling.',
      owns: 'ReAct loop, agents, prompt assembly, stop conditions',
      doesNotOwn: 'ANY domain logic (collaborators are traits)'
    }
  };

  const crateCards = document.querySelectorAll('.crate-card');
  const crateDetailTitle = document.getElementById('crateDetailTitle');
  const crateDetailDesc = document.getElementById('crateDetailDesc');
  const crateDetailCode = document.getElementById('crateDetailCode');

  crateCards.forEach(card => {
    card.addEventListener('click', () => {
      crateCards.forEach(c => c.classList.remove('active'));
      card.classList.add('active');

      const crateKey = card.getAttribute('data-crate');
      const data = cratesData[crateKey];
      if (data) {
        crateDetailTitle.textContent = data.title;
        crateDetailDesc.textContent = data.desc;
        crateDetailCode.textContent = `OWNS: ${data.owns}\nDOES NOT OWN: ${data.doesNotOwn}`;
      }
    });
  });

  // 4. Interactive Terminal Simulator
  const termBody = document.getElementById('interactiveTermBody');
  const termInput = document.getElementById('termInput');
  const sendCmdBtn = document.getElementById('sendCmdBtn');
  const clearTermBtn = document.getElementById('clearTermBtn');
  const chipBtns = document.querySelectorAll('.chip-btn');

  function appendTermLine(htmlContent) {
    const line = document.createElement('div');
    line.className = 'term-line';
    line.innerHTML = htmlContent;
    termBody.appendChild(line);
    termBody.scrollTop = termBody.scrollHeight;
  }

  function handleCommand(rawCmd) {
    const cmd = rawCmd.trim();
    if (!cmd) return;

    // Display user input line
    appendTermLine(`<span class="prompt-sym">❯</span> <span class="white">${escapeHtml(cmd)}</span>`);

    // Command Dispatch
    setTimeout(() => {
      const lower = cmd.toLowerCase();
      if (lower === 'clear') {
        termBody.innerHTML = '';
        appendTermLine('<span class="acid-text">octane agent v0.1.0</span> — Terminal cleared.');
      } else if (lower === 'help') {
        appendTermLine('<span class="yellow">AVAILABLE COMMANDS:</span>');
        appendTermLine('  <span class="cyan">octane doctor</span>     - Check configuration & kernel sandbox status');
        appendTermLine('  <span class="cyan">crates</span>            - List all workspace crates');
        appendTermLine('  <span class="cyan">sandboxtest</span>       - Simulate a blocked kernel write outside sandbox');
        appendTermLine('  <span class="cyan">octane tool grep</span>  - Simulate grep search across workspace');
        appendTermLine('  <span class="cyan">octane tool read</span>  - Simulate reading Cargo.toml');
        appendTermLine('  <span class="cyan">clear</span>             - Clear terminal screen');
      } else if (lower === 'octane doctor' || lower === 'doctor') {
        appendTermLine('<span class="dim">[1/3]</span> Checking configuration resolution...');
        appendTermLine('<span class="green">[OK]</span> Mode: hybrid | Sandbox: Seatbelt (macOS kernel active)');
        appendTermLine('<span class="dim">[2/3]</span> Testing tool containment...');
        appendTermLine('<span class="green">[OK]</span> Writable roots: [/workspace/octane-agent]');
        appendTermLine('<span class="green">[OK]</span> Read-only boundaries: [.git/, .octane/]');
        appendTermLine('<span class="dim">[3/3]</span> Checking model connectivity...');
        appendTermLine('<span class="green">[OK]</span> OpenRouter gateway connected (gemini-3.6-flash / claude-3.7-sonnet)');
        appendTermLine('<span class="acid-text">Status: ALL SYSTEMS OK (0 errors, 0 security warnings)</span>');
      } else if (lower === 'crates') {
        appendTermLine('<span class="purple">Workspace Crates (14):</span>');
        appendTermLine('  ├─ octane-protocol     (Thread / Turn / Item vocabulary)');
        appendTermLine('  ├─ octane-provider     (LanguageModel trait, pricing, stream normalization)');
        appendTermLine('  ├─ octane-tools        (8 built-in tools + MCP adapter)');
        appendTermLine('  ├─ octane-permission   (Allow / Ask / Deny policy engine)');
        appendTermLine('  ├─ octane-sandbox      (Seatbelt / Landlock / AppContainer containment)');
        appendTermLine('  ├─ octane-context      (Token budget, pruning, compaction)');
        appendTermLine('  ├─ octane-memory       (OCTANE.md layering & @imports)');
        appendTermLine('  ├─ octane-skills       (Skill discovery)');
        appendTermLine('  ├─ octane-commands     (Slash commands & templates)');
        appendTermLine('  ├─ octane-config       (.octane/ settings & agent definitions)');
        appendTermLine('  ├─ octane-mcp          (MCP JSON-RPC transport)');
        appendTermLine('  ├─ octane-core         (ReAct loop & agent coordinator)');
        appendTermLine('  ├─ octane-tui          (Ratatui streaming client)');
        appendTermLine('  └─ octane-cli          (Clap CLI parser)');
      } else if (lower.includes('sandboxtest')) {
        appendTermLine('<span class="dim">Simulating write attempt to /etc/hosts...</span>');
        appendTermLine('<span class="neon-pink">[BLOCKED BY KERNEL SANDBOX]</span> Permission denied (os error 13)');
        appendTermLine('<span class="dim">Kernel sandbox (Seatbelt) prevented write outside declared root.</span>');
      } else if (lower.includes('grep')) {
        appendTermLine('<span class="purple">⚡ [tool:grep]</span> pattern="TODO" glob="**/*.rs"');
        appendTermLine('├─ crates/octane-cli/src/main.rs: 0 matches');
        appendTermLine('├─ crates/octane-core/src/turn.rs: 0 matches');
        appendTermLine('└─ Workspace clean. 0 TODOs found.');
      } else if (lower.includes('read')) {
        appendTermLine('<span class="purple">⚡ [tool:read]</span> path="Cargo.toml" limit=10');
        appendTermLine('1  [workspace]');
        appendTermLine('2  resolver = "3"');
        appendTermLine('3  members = ["crates/*"]');
        appendTermLine('4  [workspace.package]');
        appendTermLine('5  version = "0.0.0"');
        appendTermLine('6  edition = "2024"');
        appendTermLine('7  rust-version = "1.85"');
        appendTermLine('8  license = "MIT"');
      } else {
        appendTermLine(`<span class="acid-text">Agent:</span> Received prompt "<span class="white">${escapeHtml(cmd)}</span>". Processing turn via ReAct loop...`);
        appendTermLine('<span class="green">[OK]</span> Execution complete.');
      }
    }, 150);
  }

  function escapeHtml(str) {
    return str.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  if (sendCmdBtn && termInput) {
    sendCmdBtn.addEventListener('click', () => {
      handleCommand(termInput.value);
      termInput.value = '';
    });

    termInput.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        handleCommand(termInput.value);
        termInput.value = '';
      }
    });
  }

  if (clearTermBtn) {
    clearTermBtn.addEventListener('click', () => {
      termBody.innerHTML = '';
      appendTermLine('<span class="acid-text">octane agent v0.1.0</span> — Terminal cleared.');
    });
  }

  chipBtns.forEach(chip => {
    chip.addEventListener('click', () => {
      const cmd = chip.getAttribute('data-cmd');
      if (cmd) {
        handleCommand(cmd);
      }
    });
  });

  // Quickstart Install Tabs Switcher
  const installTabs = document.querySelectorAll('.install-tab');
  installTabs.forEach(tab => {
    tab.addEventListener('click', () => {
      installTabs.forEach(t => t.classList.remove('active'));
      tab.classList.add('active');

      const target = tab.getAttribute('data-install');
      document.getElementById('install-cargo-grid').style.display = target === 'cargo' ? 'grid' : 'none';
      document.getElementById('install-nix-grid').style.display = target === 'nix' ? 'grid' : 'none';
      document.getElementById('install-source-grid').style.display = target === 'source' ? 'grid' : 'none';
    });
  });

  // Mobile Navigation Menu Toggle
  const mobileToggle = document.getElementById('mobileToggle');
  const navLinks = document.getElementById('navLinks');
  if (mobileToggle && navLinks) {
    mobileToggle.addEventListener('click', () => {
      navLinks.classList.toggle('mobile-open');
    });
  }
});
