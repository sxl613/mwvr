<script>
  (() => {
    // Theme switcher: auto / light / dark, persisted in localStorage.
    const KEY = 'mwvr_theme';
    const root = document.documentElement;
    const values = ['light', 'dark'];

    const applyTheme = (value) => {
      if (values.includes(value)) root.dataset.theme = value;
      else delete root.dataset.theme; // auto → follow system
    };

    let current = '';
    try { current = localStorage.getItem(KEY) || ''; } catch (e) {}

    const selects = document.querySelectorAll('select[data-theme-select]');
    applyTheme(current);
    selects.forEach(sel => {
      sel.value = values.includes(current) ? current : '';
      sel.addEventListener('change', () => {
        const value = sel.value;
        current = value;
        try {
          if (values.includes(value)) localStorage.setItem(KEY, value);
          else localStorage.removeItem(KEY);
        } catch (e) {}
        applyTheme(value);
        selects.forEach(other => { if (other !== sel) other.value = value; });
      });
    });
  })();
</script>