# CSSwitch 翻译代理容器镜像
#   - 纯 Python 标准库，零 pip 依赖
#   - 非 root 运行，最小攻击面
#   - 默认监听 0.0.0.0:18991（`--host` 与 `--port` 可通过 CMD 覆盖）
FROM python:3.11-slim

# 创建非 root 用户
RUN addgroup --system --gid 1001 csswitch \
    && adduser --system --uid 1001 --ingroup csswitch --no-create-home csswitch

WORKDIR /app

# 只拷代理脚本及支持文件（纯 stdlib，无 requirements.txt）
COPY proxy/csswitch_proxy.py /app/csswitch_proxy.py

# 容器内默认绑定 0.0.0.0 监听所有接口
ENV PYTHONUNBUFFERED=1
EXPOSE 18991

USER csswitch

ENTRYPOINT ["python3", "/app/csswitch_proxy.py", "--host", "0.0.0.0"]
