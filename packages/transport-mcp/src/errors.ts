/** 工具级结构化错误：code 稳定，AI 可据此自纠或向用户求助（docs/ai-design.md §3） */
export class ToolError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message)
  }
}

export const ToolErrorCode = {
  HostNotFound: 'host-not-found',
  HostKeyUnknown: 'hostkey-unknown',
  HostKeyMismatch: 'hostkey-mismatch',
  AuthFailed: 'auth-failed',
  ConnectFailed: 'connect-failed',
  SftpFailed: 'sftp-failed',
  ExecFailed: 'exec-failed',
  ExecTimeout: 'exec-timeout',
  PolicyDenied: 'policy-denied',
  ElicitDeclined: 'elicit-declined',
  ElicitUnsupported: 'elicit-unsupported',
  Disabled: 'tool-disabled',
} as const
