### Query parameters:

- **index_past**: **0** means the newest, 1 means the previous 1 picture, and so on.
- **get_image**: set to true to let it respond image directly instead of respond URL.

For example: `http://127.0.0.1:3000/?index_past=0&get_image=false`

You should get headers:
```
HTTP/1.1 200 OK
access-control-allow-origin: *
access-control-allow-headers: *
access-control-allow-method: *
```
And body:
```
https://www.bing.com/th?id=OHR.HiddenBeach_ZH-CN8410568637_1920x1080.jpg&rf=LaDigue_1920x1080.jpg&pid=hphttps://s.cn.bing.net/th?id=OHR.LionCubs_EN-CN6225017756_1920x1080.webp
```

### Notice

`www.bing.com` belongs to Microsoft.

Use of this project for purposes that violate local laws is prohibited.
