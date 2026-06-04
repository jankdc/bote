import { open, fromFile } from '@botejs/core'
import { publish } from './message-bus'

await using cursor = await open(fromFile('./some-large.json'))

for await (const orders of cursor.iter('orders', {
  batch: 10,
  select: { id: 'id', status: 'status', total: ['payment', 'total'] },
  withIndex: true,
})) {
  const messages = await Promise.all(
    orders.map(async ([index, order]) => {
      if (order.status !== 'paid') {
        return null
      }

      const restock = []
      for await (const line of cursor.walk('orders', index, 'items')) {
        if (!(await line.has('backorder'))) {
          continue
        }

        const sku = await line.get('sku')
        const short = await line.get('backorder', 'shortfall')

        const sources = []
        for await (const warehouse of line.walk('availability')) {
          const onHand = await warehouse.get()
          if (onHand > 0) {
            sources.push({ warehouse: warehouse.key, onHand })
          }
        }

        restock.push({ sku, short, sources })
      }

      return restock.length ? { id: order.id, total: order.total, restock } : null
    }),
  )

  for (const message of messages) {
    if (message) {
      await publish('orders.fulfil', message)
    }
  }
}
