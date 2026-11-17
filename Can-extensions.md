CAN 扩展任务提交说明（任务 1 + 任务 2）

编译：cargo build --locked；工具链 1.99.1 / 目标 thumbv7em-none-eabihf；
      生成 dm-mc02-cmsis-dap ELF，完整输出见仓库 build-success.log。

入口 / 数据路径
- 入口：wiring.rs 用 can1_fd 初始化 FDCAN1（CAN FD + BRS + TDC），再 buffered_fd 拆成
  可共享的 can_tx/can_rx；main.rs 用 join4 并发运行 USB 设备、控制串口、UART 串口、CMSIS-DAP。
- 任务 1（Classical CAN 双向桥接）：FDCAN 中断把帧放入 RxFdBuf(32) → 控制串口任务 select
  收到 → format_frame 序列化并主动写回主机（RX …）；主机 CAN TX <id> <hex> → execute →
  can_tx.write → 发送。
- 任务 2（CAN FD）：CANFD TX <id> <len> <F|B> <hex> 构造 FdFrame（支持 12/32/64 字节），
  转发为 RXFD …。

关键配置（FDCAN 内核 80 MHz）
- 仲裁段 1 Mbit/s：prescaler=1，seg1=59，seg2=20，SJW=20（80 tq，采样点 75%）。
- 数据段 5 Mbit/s：prescaler=1，seg1=11，seg2=4，SJW=4（16 tq，采样点 75%），BRS 打开。
- TDC：直接写 TDCR，TDCO=10 mtq（≈125 ns，覆盖 MCP2542FD TX→RX 环路延迟），TDCF=0。

