// Octane Website Script
document.addEventListener('DOMContentLoaded', () => {
  // Copy to Clipboard Functionality
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
          }, 1800);
        }).catch(err => {
          console.error('Copy failed:', err);
        });
      }
    });
  });
});
