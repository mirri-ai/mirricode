import { withBase } from 'vitepress'

// Resolve a link written in homepage frontmatter into a routable URL.
// Locale-relative links ("guides/x") gain the current locale prefix, absolute
// paths ("/zh/x") and external URLs are used as written.
export function resolveLink(link: string | undefined, relativePath: string): string {
  if (!link) return ''
  if (/^https?:\/\//.test(link)) return link
  if (link.startsWith('/')) return withBase(link)

  const locale = relativePath.split('/')[0]
  const prefix = locale === 'en' || locale === 'zh' ? `/${locale}/` : '/'
  return withBase(`${prefix}${link}`)
}
