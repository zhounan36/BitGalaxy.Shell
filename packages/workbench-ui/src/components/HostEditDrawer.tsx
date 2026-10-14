import { useEffect, useRef, useState } from 'react'
import { Alert, Button, Drawer, Form, Input, InputNumber, Radio } from 'antd'
import { ApiOutlined, FolderOpenOutlined } from '@ant-design/icons'
import type { HostInput, HostRecord } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'

/** 仅 Tauri 壳内有原生文件选择器；浏览器宿主无法取得本机真实路径 */
const isTauri = '__TAURI_INTERNALS__' in window

export interface HostEditDrawerProps {
  open: boolean
  host?: HostRecord
  onClose: () => void
  onSaved: () => void
}

/** 主机档案新增/编辑（M1：凭据内联 hosts.json；M2 起改由密库托管凭据引用） */
export function HostEditDrawer({ open, host, onClose, onSaved }: HostEditDrawerProps) {
  const [form] = Form.useForm()
  const [testing, setTesting] = useState(false)
  const [testResult, setTestResult] = useState<{ ok: boolean; message: string } | null>(null)
  const testTimer = useRef<number | undefined>(undefined)

  useEffect(() => {
    if (!open) return
    form.resetFields()
    setTesting(false)
    setTestResult(null)
    if (host) {
      form.setFieldsValue({
        name: host.name,
        host: host.host,
        port: host.port,
        username: host.username,
        authKind: host.auth.kind,
        password: host.auth.password,
        privateKeyPath: host.auth.privateKeyPath,
        passphrase: host.auth.passphrase,
      })
    }
  }, [open, host, form])

  // 测试连接回包：展示结果并复位 loading；打开抽屉期间保持订阅，关闭时清理
  useEffect(() => {
    if (!open) return
    const off = agentSocket.onControl((frame) => {
      if (frame.t !== 'test-host-result') return
      window.clearTimeout(testTimer.current)
      setTesting(false)
      setTestResult({ ok: frame.m.ok, message: frame.m.message ?? (frame.m.ok ? '认证成功' : '认证失败') })
    })
    return () => {
      off()
      window.clearTimeout(testTimer.current)
    }
  }, [open])

  /** 表单值 → HostInput（保存与测试连接共用） */
  const buildInput = (v: Record<string, unknown>): HostInput => {
    const auth =
      v.authKind === 'password'
        ? { kind: 'password' as const, password: v.password as string }
        : { kind: 'privateKey' as const, privateKeyPath: v.privateKeyPath as string, passphrase: v.passphrase as string }
    return {
      name: v.name as string,
      host: v.host as string,
      port: (v.port as number) ?? 22,
      username: v.username as string,
      auth,
    }
  }

  const save = async () => {
    try {
      const v = await form.validateFields()
      const input = buildInput(v)
      agentSocket.sendControl(
        host
          ? { t: 'hosts', m: { op: 'update', host: { ...input, id: host.id } } }
          : { t: 'hosts', m: { op: 'create', host: input } },
      )
      onSaved()
    } catch {
      // 校验未通过：antd 已内联展示错误，不提交
    }
  }

  /** 测试连接：用表单当前凭据做一次真实 SSH 认证（不保存档案） */
  const testConnection = async () => {
    try {
      const v = await form.validateFields()
      setTesting(true)
      setTestResult(null)
      agentSocket.sendControl({ t: 'test-host', m: buildInput(v) })
      testTimer.current = window.setTimeout(() => {
        setTesting(false)
        setTestResult({ ok: false, message: '测试超时（45 秒无响应），请检查主机地址与端口' })
      }, 45_000)
    } catch {
      // 校验未通过：antd 已内联展示错误，不测试
    }
  }

  /** 系统文件选择器选取私钥文件（私钥常无扩展名，不做类型过滤） */
  const pickPrivateKey = async () => {
    const { open } = await import('@tauri-apps/plugin-dialog')
    const picked = await open({ multiple: false, title: '选择私钥文件' })
    if (typeof picked === 'string' && picked) form.setFieldValue('privateKeyPath', picked)
  }

  return (
    <Drawer
      title={host ? '编辑主机' : '添加主机'}
      open={open}
      onClose={onClose}
      width={440}
      footer={
        <div style={{ display: 'flex', justifyContent: 'space-between', gap: 8 }}>
          <Button icon={<ApiOutlined />} loading={testing} onClick={() => void testConnection()}>
            测试连接
          </Button>
          <div style={{ display: 'flex', gap: 8 }}>
            <Button onClick={onClose}>取消</Button>
            <Button type="primary" onClick={() => void save()}>
              保存
            </Button>
          </div>
        </div>
      }
    >
      <Form form={form} layout="vertical" scrollToFirstError initialValues={{ port: 22, authKind: 'password' }}>
        <Form.Item
          name="name"
          label="名称"
          rules={[
            { required: true, message: '请输入名称' },
            { whitespace: true, message: '名称不能为空白' },
          ]}
        >
          <Input placeholder="如：生产跳板机" />
        </Form.Item>
        <Form.Item
          name="host"
          label="主机地址"
          rules={[
            { required: true, message: '请输入主机地址' },
            { whitespace: true, message: '主机地址不能为空白' },
          ]}
        >
          <Input placeholder="IP 或域名" />
        </Form.Item>
        <Form.Item
          name="port"
          label="端口"
          rules={[
            { required: true, message: '请输入端口' },
            { type: 'integer', min: 1, max: 65535, message: '端口需为 1–65535 的整数' },
          ]}
        >
          <InputNumber min={1} max={65535} style={{ width: '100%' }} />
        </Form.Item>
        <Form.Item
          name="username"
          label="用户名"
          rules={[
            { required: true, message: '请输入用户名' },
            { whitespace: true, message: '用户名不能为空白' },
          ]}
        >
          <Input />
        </Form.Item>
        <Form.Item name="authKind" label="认证方式">
          <Radio.Group
            options={[
              { value: 'password', label: '密码' },
              { value: 'privateKey', label: '私钥' },
            ]}
            optionType="button"
          />
        </Form.Item>
        <Form.Item noStyle shouldUpdate>
          {({ getFieldValue }) =>
            getFieldValue('authKind') === 'password' ? (
              <Form.Item name="password" label="密码" rules={[{ required: true, message: '请输入密码' }]}>
                <Input.Password />
              </Form.Item>
            ) : (
              <>
                <Form.Item
                  name="privateKeyPath"
                  label="私钥文件路径"
                  rules={[{ required: true, message: '请输入私钥文件路径' }]}
                  tooltip="M1 直连版：Agent 直接读本机私钥文件（M2 起由密库托管）"
                >
                  <Input
                    placeholder="C:\Users\me\.ssh\id_ed25519"
                    addonAfter={
                      isTauri ? (
                        <Button
                          type="text"
                          size="small"
                          icon={<FolderOpenOutlined />}
                          onClick={() => void pickPrivateKey()}
                        >
                          浏览
                        </Button>
                      ) : undefined
                    }
                  />
                </Form.Item>
                <Form.Item name="passphrase" label="私钥口令（可选）">
                  <Input.Password />
                </Form.Item>
              </>
            )
          }
        </Form.Item>
      </Form>
      {testResult && (
        <Alert
          style={{ marginTop: 16 }}
          type={testResult.ok ? 'success' : 'error'}
          showIcon
          message={testResult.ok ? '测试连接成功' : '测试连接失败'}
          description={testResult.message}
        />
      )}
    </Drawer>
  )
}
