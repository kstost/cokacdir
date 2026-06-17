import { Link } from 'react-router-dom'
import { useLanguage } from '../LanguageContext'

interface Props {
  activeSectionId: string
  onNavigate: () => void
}

interface SidebarItem {
  id: string
  en: string
  ko: string
}

interface SidebarCategory {
  en: string
  ko: string
  items: SidebarItem[]
}

const categories: SidebarCategory[] = [
  {
    en: 'Getting Started',
    ko: '시작하기',
    items: [
      { id: 'install-windows', en: 'Install on Windows', ko: 'Windows에 설치하기' },
      { id: 'install-macos', en: 'Install on macOS', ko: 'macOS에 설치하기' },
      { id: 'install-linux', en: 'Install on Linux', ko: 'Linux에 설치하기' },
      { id: 'install-ec2', en: 'Install on AWS EC2', ko: 'AWS EC2에 설치하기' },
      { id: 'telegram-bot', en: 'Telegram Bot Setup', ko: '텔레그램 봇 설정' },
      { id: 'discord-bot', en: 'Discord Bot Setup', ko: '디스코드 봇 설정' },
      { id: 'slack-bot', en: 'Slack Bot Setup', ko: 'Slack 봇 설정' },
      { id: 'token-management', en: 'Token Management', ko: '토큰 관리' },
      { id: 'first-chat', en: 'First Chat', ko: '첫 번째 채팅' },
      { id: 'update', en: 'Updating', ko: '업데이트' },
    ],
  },
  {
    en: 'Usage',
    ko: '사용법',
    items: [
      { id: 'sessions', en: 'Session Management', ko: '세션 관리' },
      { id: 'requests', en: 'Request Management', ko: '요청 관리' },
      { id: 'file-transfer', en: 'File Transfer', ko: '파일 전송' },
      { id: 'shell-commands', en: 'Shell Commands', ko: '셸 명령어' },
      { id: 'schedules', en: 'Schedules', ko: '예약 작업' },
    ],
  },
  {
    en: 'Advanced',
    ko: '고급',
    items: [
      { id: 'group-chat', en: 'Group Chat', ko: '그룹 채팅' },
      { id: 'multiple-chats', en: 'Multiple Chats with One Bot', ko: '하나의 봇으로 여러 채팅' },
      { id: 'tool-management', en: 'Tool Management', ko: '도구 관리' },
      { id: 'agy-provider', en: 'Agy Provider', ko: 'Agy 프로바이더' },
      { id: 'instructions', en: 'Custom Instructions', ko: '커스텀 지시사항' },
      { id: 'settings', en: 'Settings', ko: '설정' },
      { id: 'env-vars', en: 'Environment Variables', ko: '환경변수' },
    ],
  },
]

export default function DocsSidebar({ activeSectionId, onNavigate }: Props) {
  const { t } = useLanguage()
  return (
    <nav className="space-y-6">
      {categories.map((cat) => (
        <div key={cat.en}>
          <h3 className="text-xs font-semibold text-zinc-500 uppercase tracking-wider mb-2 px-2">
            {t(cat.en, cat.ko)}
          </h3>
          <ul className="space-y-0.5">
            {cat.items.map((item) => (
              <li key={item.id}>
                <Link
                  to={`/docs/${item.id}`}
                  onClick={onNavigate}
                  className={`
                    block px-3 py-1.5 rounded-md text-sm transition-colors
                    ${
                      activeSectionId === item.id
                        ? 'bg-accent-cyan/10 text-accent-cyan font-medium'
                        : 'text-zinc-400 hover:text-white hover:bg-zinc-800/50'
                    }
                  `}
                >
                  {t(item.en, item.ko)}
                </Link>
              </li>
            ))}
          </ul>
        </div>
      ))}
    </nav>
  )
}

// Export for prev/next navigation
export function getAllSections(): { id: string; en: string; ko: string }[] {
  return categories.flatMap((cat) => cat.items)
}
