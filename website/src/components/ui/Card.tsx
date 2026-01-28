import { motion } from 'framer-motion'
import { ReactNode } from 'react'

interface CardProps {
  children: ReactNode
  className?: string
}

export default function Card({ children, className = '' }: CardProps) {
  return (
    <motion.div
      className={`bg-bg-card border border-zinc-800 rounded-xl p-6 ${className}`}
      whileHover={{
        borderColor: 'rgba(0, 212, 255, 0.3)',
        boxShadow: '0 0 30px rgba(0, 212, 255, 0.1)'
      }}
      transition={{ duration: 0.3 }}
    >
      {children}
    </motion.div>
  )
}
