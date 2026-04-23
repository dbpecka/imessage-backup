<script lang="ts">
  import '../app.css';

  let { children } = $props();
</script>

<div class="app-bg" aria-hidden="true"><span></span></div>

<main>
  <header class="hero">
    <img class="hero-mark" src="/icon.png" alt="Bubble Wrap" aria-hidden="true" />
    <div>
      <div class="hero-title">Bubble Wrap</div>
      <div class="hero-sub">Reclaim your disk space! Export and prune your Messages — safely.</div>
    </div>
  </header>
  {@render children()}
</main>

<style>
  main {
    max-width: 760px;
    margin: 0 auto;
    padding: 40px 28px 80px;
  }

  .hero {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-bottom: 28px;
  }

  .hero-mark {
    width: 48px;
    height: 48px;
    border-radius: 12px;
    box-shadow: var(--shadow-card);
    flex-shrink: 0;
  }

  .hero-title {
    font-size: 20px;
    font-weight: 650;
    letter-spacing: -0.01em;
  }

  .hero-sub {
    color: var(--text-muted);
    font-size: 13px;
  }
</style>
