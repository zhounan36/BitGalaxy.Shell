import { useEffect, useMemo } from 'react'
import { Alert, Button, Collapse, Form, Modal, Select, Space, Switch, Typography } from 'antd'
import { ReloadOutlined, UsbOutlined } from '@ant-design/icons'
import type { SerialOptions, SerialPortInfo } from '@bgshell/protocol'

const BAUD_RATES = [9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600]

export interface SerialConnectModalProps {
  open: boolean
  ports: SerialPortInfo[]
  error?: string
  onClose: () => void
  onRefresh: () => void
  onConnect: (opts: SerialOptions) => void
}

/** 串口连接弹窗：枚举本机端口（serial-list → serial-ports）+ 波特率 + 高级参数（8N1 缺省）。
 *  打开失败（占用/不存在）由后端 fail_open 以 open-failed 呈现，页签自动移除。 */
export function SerialConnectModal({ open, ports, error, onClose, onRefresh, onConnect }: SerialConnectModalProps) {
  const [form] = Form.useForm<SerialOptions>()

  useEffect(() => {
    if (open) form.resetFields()
  }, [open, form])

  const portOptions = useMemo(
    () =>
      ports.map((p) => ({
        value: p.name,
        label: p.description ? `${p.name} · ${p.description}` : p.name,
      })),
    [ports],
  )

  return (
    <Modal
      open={open}
      title={
        <Space>
          <UsbOutlined />
          连接串口
        </Space>
      }
      okText="连接"
      cancelText="取消"
      okButtonProps={{ disabled: ports.length === 0 }}
      onCancel={onClose}
      onOk={() => {
        void form.validateFields().then((v) => onConnect(v))
      }}
    >
      <Form
        form={form}
        layout="vertical"
        initialValues={{
          baudRate: 115200,
          dataBits: 8,
          parity: 'none',
          stopBits: 1,
          flowControl: 'none',
          dtr: true,
          rts: true,
        }}
      >
        <Form.Item name="port" label="端口" rules={[{ required: true, message: '请选择串口' }]}>
          <Select
            showSearch
            placeholder={ports.length ? '选择串口' : '未检测到串口'}
            options={portOptions}
            optionFilterProp="label"
            notFoundContent={
              <Button type="link" size="small" icon={<ReloadOutlined />} onClick={onRefresh}>
                未检测到串口，点击刷新
              </Button>
            }
            suffixIcon={
              <ReloadOutlined
                onClick={(e) => {
                  e.stopPropagation()
                  onRefresh()
                }}
              />
            }
          />
        </Form.Item>
        <Form.Item name="baudRate" label="波特率" rules={[{ required: true }]}>
          <Select
            showSearch
            options={BAUD_RATES.map((b) => ({ value: b, label: String(b) }))}
            filterOption={(input, option) => String(option?.value ?? '').includes(input)}
          />
        </Form.Item>
        <Collapse
          ghost
          items={[
            {
              key: 'adv',
              label: <Typography.Text type="secondary">高级参数（缺省 8-N-1 无流控）</Typography.Text>,
              children: (
                <>
                  <Form.Item name="dataBits" label="数据位" style={{ marginBottom: 12 }}>
                    <Select
                      options={[5, 6, 7, 8].map((v) => ({ value: v, label: String(v) }))}
                    />
                  </Form.Item>
                  <Form.Item name="parity" label="校验位" style={{ marginBottom: 12 }}>
                    <Select
                      options={[
                        { value: 'none', label: '无 (N)' },
                        { value: 'odd', label: '奇校验 (O)' },
                        { value: 'even', label: '偶校验 (E)' },
                      ]}
                    />
                  </Form.Item>
                  <Form.Item name="stopBits" label="停止位" style={{ marginBottom: 12 }}>
                    <Select options={[1, 2].map((v) => ({ value: v, label: String(v) }))} />
                  </Form.Item>
                  <Form.Item name="flowControl" label="流控" style={{ marginBottom: 12 }}>
                    <Select
                      options={[
                        { value: 'none', label: '无' },
                        { value: 'hardware', label: '硬件 RTS/CTS' },
                        { value: 'software', label: '软件 XON/XOFF' },
                      ]}
                    />
                  </Form.Item>
                  <Space size={16}>
                    <Form.Item name="dtr" label="打开时 DTR" valuePropName="checked" style={{ marginBottom: 0 }}>
                      <Switch />
                    </Form.Item>
                    <Form.Item name="rts" label="打开时 RTS" valuePropName="checked" style={{ marginBottom: 0 }}>
                      <Switch />
                    </Form.Item>
                  </Space>
                  <Typography.Text type="secondary" style={{ display: 'block', marginTop: 4, fontSize: 12 }}>
                    连接瞬间的 DTR/RTS 电平：Arduino 会因 DTR 跳变自动复位，调试时可关闭
                  </Typography.Text>
                </>
              ),
            },
          ]}
        />
      </Form>
      {error && <Alert type="error" showIcon message={error} style={{ marginTop: 8 }} />}
    </Modal>
  )
}
