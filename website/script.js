// Octane Interactive Website Functionality
document.addEventListener('DOMContentLoaded', () => {
  
  // 1. Interactive Tabs Mechanism
  const tabBtns = document.querySelectorAll('.tab-btn');
  const tabContents = document.querySelectorAll('.tab-content');

  tabBtns.forEach(btn => {
    btn.addEventListener('click', () => {
      const targetTab = btn.getAttribute('data-tab');

      // Update active state on buttons
      tabBtns.forEach(b => b.classList.remove('active'));
      btn.classList.add('active');

      // Update active state on contents
      tabContents.forEach(content => {
        content.classList.remove('active');
        if (content.id === `tab-${targetTab}`) {
          content.classList.add('active');
        }
      });
    });
  });

  // 2. Clipboard Copy Functionality with Visual Feedback
  const copyBtns = document.querySelectorAll('.copy-btn');
  copyBtns.forEach(btn => {
    btn.addEventListener('click', () => {
      const textToCopy = btn.getAttribute('data-copy');
      if (textToCopy) {
        navigator.clipboard.writeText(textToCopy).then(() => {
          const labelSpan = btn.querySelector('.copy-label');
          const originalText = labelSpan ? labelSpan.textContent : btn.textContent;
          
          if (labelSpan) {
            labelSpan.textContent = 'Copied!';
          } else {
            btn.textContent = 'Copied!';
          }

          btn.style.borderColor = 'rgba(48, 209, 88, 0.6)';
          btn.style.color = '#30d158';

          setTimeout(() => {
            if (labelSpan) {
              labelSpan.textContent = originalText;
            } else {
              btn.textContent = originalText;
            }
            btn.style.borderColor = '';
            btn.style.color = '';
          }, 2000);
        }).catch(err => {
          console.error('Failed to copy text:', err);
        });
      }
    });
  });

  // 3. Smooth Scroll Navbar Shadow on Scroll
  const navbar = document.getElementById('navbar');
  window.addEventListener('scroll', () => {
    if (window.scrollY > 20) {
      navbar.style.borderBottomColor = 'rgba(255, 255, 255, 0.15)';
      navbar.style.boxShadow = '0 10px 30px rgba(0, 0, 0, 0.8)';
    } else {
      navbar.style.borderBottomColor = 'rgba(255, 255, 255, 0.08)';
      navbar.style.boxShadow = 'none';
    }
  });

  // 4. Scroll Reveal Intersection Observer
  const revealElements = document.querySelectorAll('.reveal-on-scroll');
  const revealObserver = new IntersectionObserver((entries, observer) => {
    entries.forEach(entry => {
      if (entry.isIntersecting) {
        entry.target.classList.add('revealed');
        observer.unobserve(entry.target);
      }
    });
  }, {
    root: null,
    threshold: 0.1,
    rootMargin: '0px 0px -40px 0px'
  });

  revealElements.forEach(el => revealObserver.observe(el));

  // 5. Hero 3D Interactive Mouse Tilt
  const heroWrapper = document.querySelector('.hero-visual-wrapper');
  const macWindow = document.querySelector('.mac-window');

  if (heroWrapper && macWindow) {
    heroWrapper.addEventListener('mousemove', (e) => {
      const rect = heroWrapper.getBoundingClientRect();
      const x = e.clientX - rect.left;
      const y = e.clientY - rect.top;
      
      const centerX = rect.width / 2;
      const centerY = rect.height / 2;
      
      const rotateX = ((y - centerY) / centerY) * -6; // max 6 deg tilt
      const rotateY = ((x - centerX) / centerX) * 6;  // max 6 deg tilt
      
      macWindow.style.transform = `rotateX(${rotateX}deg) rotateY(${rotateY}deg)`;
    });

    heroWrapper.addEventListener('mouseleave', () => {
      macWindow.style.transform = 'rotateX(0deg) rotateY(0deg)';
    });
  }

});
