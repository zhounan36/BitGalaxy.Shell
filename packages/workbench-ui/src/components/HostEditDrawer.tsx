import { useEffect } from 'react'
import { Button, Drawer, Form, Input, InputNumber, Radio } from 'antd'
import type { HostInput, HostRecord } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'

export interface HostEditDrawerProps {
  open: boolean
  host?: HostRecord
  onClose: () => void
  onSaved: () => void
}

/** 主机档案新增/编辑（M1：凭据内联 hosts.json；M2 起改由密库托管凭据引用） */
export function HostEditDrawer({ open, host, onClose, onSaved }: HostEditDrawerProps) {
  const [form] = Form.useForm()

  useEffect(() => {
    if (!open) return
    form.resetFields()
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

  const save = async () => {
    const v = await form.validateFields()
    const input: HostInput = {
      name: v.name as string,
      host: v.host as string,
      port: (v.port as number) ?? 22,
      username: v.username as string,
      auth:
        v.authKind === 'password'
          ? { kind: 'password', password: v.password as string }
          : { kind: 'privateKey', privateKeyPath: v.privateKeyPath as string, passphrase: v.passphrase as string },
    }
    agentSocket.sendControl(
      host
        ? { t: 'hosts', m: { op: 'update', host: { ...input, id: host.id } } }
        : { t: 'hosts', m: { op: 'create', host: input } },
    )
    onSaved()
  }

  return (
    <Drawer
      title={host ? '编辑主机' : '添加主机'}
      open={open}
      onClose={onClose}
      width={440}
      footer={
        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
          <Button onClick={onClose}>取消</Button>
          <Button type="primary" onClick={() => void save()}>
            保存
          </Button>
        </div>
      }
    >
      <Form form={form} layout="vertical" initialValues={{ port: 22, authKind: 'password' }}>
        <Form.Item name="name" label="名称" rules={[{ required: true, message: '请输入名称' }]}>
          <Input placeholder="如：生产跳板机" />
        </Form.Item>
        <Form.Item name="host" label="主机地址" rules={[{ required: true, message: '请输入主机地址' }]}>
          <Input placeholder="IP 或域名" />
        </Form.Item>
        <Form.Item name="port" label="端口" rules={[{ required: true, message: '请输入端口' }]}>
          <InputNumber min={1} max={65535} style={{ width: '100%' }} />
        </Form.Item>
        <Form.Item name="username" label="用户名" rules={[{ required: true, message: '请输入用户名' }]}>
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
                  <Input placeholder="C:\Users\me\.ssh\id_ed25519" />
                </Form.Item>
                <Form.Item name="passphrase" label="私钥口令（可选）">
                  <Input.Password />
                </Form.Item>
              </>
            )
          }
        </Form.Item>
      </Form>
    </Drawer>
  )
}
