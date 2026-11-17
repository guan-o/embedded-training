CAN 扩展任务提交说明

编译：cargo build --locked；工具链 1.99.1 / 目标 thumbv7em-none-eabihf；
      生成 dm-mc02-cmsis-dap ELF，完整输出见仓库 build-success.log。

入口 / 数据路径
- 入口：wiring.rs 用 can1_fd 初始化 FDCAN1，再 buffered_fd 拆成可共享的
  can_tx/can_rx；main.rs 用 join5 并发跑 USB、控制串口、UART、CMSIS-DAP、gs_usb。
- Classical CAN 桥接：FDCAN 中断把帧放入 RxFdBuf(32) → 控制串口任务 select 收到 →
  format_frame 序列化并主动写回主机（RX …）；主机 CAN TX … → execute → can_tx.write → 发送。
- CAN FD：CANFD TX <id> <len> <F|B> <hex> 构造 FdFrame（支持 12/32/64 字节），转发为 RXFD …。
- SocketCAN：gs_usb.rs 实现厂商接口 + bulk IN/OUT 及控制请求
  （HOST_FORMAT / BITTIMING / MODE / BT_CONST / DEVICE_CONFIG / GET_STATE），
  主机帧 ↔ FdFrame 双向转换，Linux 端 gs_usb 驱动映射为 can0。

关键配置（FDCAN 内核 80 MHz）
- 仲裁段 1 Mbit/s：prescaler=1，seg1=59，seg2=20，SJW=20（80 tq，采样点 75%）。
- 数据段 5 Mbit/s：prescaler=1，seg1=11，seg2=4，SJW=4（16 tq，采样点 75%），BRS 打开。
- TDC：直接写 TDCR，TDCO=10 mtq（≈125 ns，覆盖 MCP2542FD TX→RX 环路延迟），TDCF=0。

状态：固件已通过目标编译与链接；硬件行为为预期，尚未上板验证。
