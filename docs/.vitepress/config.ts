import { defineConfig } from 'vitepress'

// ── Replace these per-project ──────────────────────────────────────────────
const projectName = 'elivagar'
const projectDescription = 'Shortbread vector tile generator'
const githubUrl = 'https://github.com/folknor/elivagar'
const base = '/elivagar/'
// ────────────────────────────────────────────────────────────────────────────

export default defineConfig({
  title: projectName,
  description: projectDescription,
  base,

  appearance: 'dark',

  head: [
    ['link', { rel: 'icon', type: 'image/svg+xml', href: `${base}elivagar-logo.svg` }],
  ],

  themeConfig: {
    // Wordmark in the header, so the nav title text is suppressed - otherwise
    // it reads "elivagar" twice. The hero on the home page uses the icon-only
    // pair instead, since hero.name already supplies the wordmark there.
    logo: {
      light: '/elivagar-logo-text.svg',
      dark: '/elivagar-logo-text-dark.svg',
    },
    siteTitle: false,

    nav: [
      { text: 'Guide', link: '/guide/' },
      { text: 'Reference', link: '/reference/cli' },
      { text: 'API Docs', link: 'https://docs.rs/elivagar' },
    ],

    search: {
      provider: 'local',
    },

    socialLinks: [
      { icon: 'github', link: githubUrl },
    ],

    footer: {
      message: `Released under MIT or Apache-2.0, at your option. | Copyright folk@folk.wtf`,
    },

    sidebar: {
      '/guide/': [
        {
          text: 'Guide',
          items: [
            { text: 'Getting Started', link: '/guide/' },
            { text: 'Installation', link: '/guide/install' },
            { text: 'Ocean Input', link: '/guide/ocean' },
            { text: 'The Pipeline', link: '/guide/pipeline' },
            { text: 'Correctness Gates', link: '/guide/correctness' },
            { text: 'Performance', link: '/guide/performance' },
          ],
        },
      ],
      '/reference/': [
        {
          text: 'Reference',
          items: [
            { text: 'CLI', link: '/reference/cli' },
            { text: 'Corpus Gate', link: '/reference/corpus' },
            { text: 'Archive Metadata', link: '/reference/metadata' },
            { text: 'Performance Record', link: '/reference/performance' },
          ],
        },
      ],
    },
  },
})
