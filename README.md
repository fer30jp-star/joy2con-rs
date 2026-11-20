# joy2con-rs

Driver y app para usar los **Joy-Con de Switch 2** (y clones compatibles, como los
NYXI Hyperion 3 Ultra) en Linux por Bluetooth. Es un port a Rust de
[jacc/joy2con](https://github.com/jacc/joy2con) (Python), con un **modo ratón** nuevo.

- Empareja cada mitad con la PC y la reconecta al presionar cualquier botón.
- Las presenta a los juegos como un **Pro Controller** original de Switch (o dos
  Joy-Con separados), con giroscopio, vibración HD y luces de jugador. El driver
  `hid-nintendo` del kernel, Steam, SDL y Wine ya los entienden.
- **Modo ratón**: usa el sensor óptico de un Joy-Con como ratón (uinput).
  L/R = clic izquierdo, ZL/ZR = clic derecho, presionar el stick = clic central,
  stick = rueda. Mantén **SL + SR** un segundo para activarlo o desactivarlo.
- App de escritorio (egui) en español con dibujos en vivo, giroscopio,
  diagnóstico e icono en la bandeja de KDE.

## Instalar

Necesitas Rust (`cargo`) y BlueZ 5.66 o más nuevo.

```bash
./install.sh                              # compila e instala en ~/.local
joy2con setup                             # ajustes de Bluetooth (pide contraseña)
systemctl --user enable --now joy2con     # el driver, ahora y en cada inicio de sesión
joy2con                                   # la app
```

Si el driver no puede crear el mando o el ratón virtual, da acceso a
`/dev/uhid` y `/dev/uinput` una sola vez:

```bash
sudo install -Dm644 data/70-joy2con.rules /etc/udev/rules.d/70-joy2con.rules
sudo udevadm control --reload && sudo udevadm trigger
```

### Adaptadores Bluetooth delicados

Algunos adaptadores (por ejemplo UGREEN BT5.4) se reinician si se les pide
una conexión justo al terminar de escanear o con un intervalo de 7.5 ms:

```bash
joy2con setup --interval-ms 15
mkdir -p ~/.config/systemd/user/joy2con.service.d
printf '[Service]\nEnvironment=JOY2CON_CONNECT_DELAY=1.5\n' > ~/.config/systemd/user/joy2con.service.d/delay.conf
systemctl --user daemon-reload && systemctl --user restart joy2con
```

## Comandos

```
joy2con                    abre la app
joy2con app --hidden       abre la app solo en la bandeja
joy2con daemon [-v]        el driver (normalmente el servicio de systemd)
joy2con status             lo que ve el driver
joy2con setup [--interval-ms 7.5|15]
```

## Créditos y licencia

Basado en [joy2con](https://github.com/jacc/joy2con) de Jack LaFond (protocolo,
emulación y dibujos). El formato del sensor óptico viene de
[jc2mouse](https://github.com/coffincolors/jc2mouse). GPL-3.0-or-later, igual
que el original. No está afiliado con Nintendo.
