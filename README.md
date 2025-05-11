Build local:
docker build -t airies-dl .
Run Local:
docker run -d --name airies-dl -p 8000:8000 -v C:\test\www:/www:ro airies-dl